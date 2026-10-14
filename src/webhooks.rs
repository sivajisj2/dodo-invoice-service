use axum::{Extension, Json, extract::State, http::StatusCode};
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;
use sqlx::FromRow;
use std::time::Duration;
use uuid::Uuid;

use crate::state::AppState;

type HmacSha256 = Hmac<Sha256>;

const MAX_ATTEMPTS: i32 = 8;
const BATCH_SIZE: i64 = 10;

#[derive(Debug, Serialize)]
pub struct CreateWebhookEndpointResponse {
    pub id: Uuid,
    pub url: String,
    pub enabled: bool,
    pub secret: String,
    pub message: &'static str,
}

#[derive(Debug, FromRow)]
struct DueDelivery {
    delivery_id: Uuid,
    attempt_count: i32,
    payload: Value,
    url: String,
    secret: String,
}

/// Register an endpoint for the authenticated business.
/// The signing secret is returned once in this response.
pub async fn create_endpoint(
    State(state): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Json(payload): Json<Value>,
) -> Result<(StatusCode, Json<CreateWebhookEndpointResponse>), StatusCode> {
    let url = payload
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or(StatusCode::BAD_REQUEST)?;

    let parsed_url = reqwest::Url::parse(url).map_err(|_| StatusCode::BAD_REQUEST)?;

    if !matches!(parsed_url.scheme(), "http" | "https") || parsed_url.host_str().is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let endpoint_id = Uuid::new_v4();

    // Generate a high-entropy secret. Store it securely in production;
    // this demo stores it in the existing secret column.
    let secret = format!("whsec_{}_{}", Uuid::new_v4(), Uuid::new_v4());

    sqlx::query(
        r#"
        INSERT INTO webhook_endpoints (id, business_id, url, secret)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(endpoint_id)
    .bind(business_id)
    .bind(url)
    .bind(&secret)
    .execute(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(?error, "failed to create webhook endpoint");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok((
        StatusCode::CREATED,
        Json(CreateWebhookEndpointResponse {
            id: endpoint_id,
            url: url.to_string(),
            enabled: true,
            secret,
            message: "Store this signing secret securely",
        }),
    ))
}

/// Continuously processes due deliveries.
/// Run one worker loop per service process; database row locking permits
/// multiple service instances to share the same queue.
pub async fn run_worker(state: AppState) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(?error, "failed to build webhook HTTP client");
            return;
        }
    };

    loop {
        match claim_due_deliveries(&state).await {
            Ok(deliveries) => {
                for delivery in deliveries {
                    deliver_one(&state, &client, delivery).await;
                }
            }
            Err(error) => {
                tracing::error!(?error, "failed to claim webhook deliveries");
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Atomically claims a small batch. SKIP LOCKED allows other worker
/// processes to claim different rows instead of waiting on these rows.
async fn claim_due_deliveries(state: &AppState) -> Result<Vec<DueDelivery>, sqlx::Error> {
    sqlx::query_as::<_, DueDelivery>(
        r#"
        
WITH candidates AS (
    SELECT
        d.id,
        d.event_id,
        d.endpoint_id
    FROM webhook_deliveries AS d
    JOIN webhook_events AS e ON e.id = d.event_id
    JOIN webhook_endpoints AS ep ON ep.id = d.endpoint_id
    WHERE ep.enabled = TRUE
      AND (
          (d.status = 'pending' AND d.next_attempt_at <= NOW())
          OR
          (
              d.status = 'delivering'
              AND d.updated_at < NOW() - INTERVAL '5 minutes'
          )
      )
    ORDER BY d.next_attempt_at, d.created_at
    LIMIT $1
    FOR UPDATE OF d SKIP LOCKED
)
UPDATE webhook_deliveries AS d
SET status = 'delivering',
    attempt_count = d.attempt_count + 1,
    updated_at = NOW()
FROM candidates AS c
JOIN webhook_events AS e ON e.id = c.event_id
JOIN webhook_endpoints AS ep ON ep.id = c.endpoint_id
WHERE d.id = c.id
RETURNING
    d.id AS delivery_id,
    d.attempt_count,
    e.payload,
    ep.url,
    ep.secret

        "#,
    )
    .bind(BATCH_SIZE)
    .fetch_all(&state.db)
    .await
}

async fn deliver_one(state: &AppState, client: &reqwest::Client, delivery: DueDelivery) {
    let body = match serde_json::to_vec(&delivery.payload) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(
                delivery_id = %delivery.delivery_id,
                ?error,
                "failed to serialize webhook payload"
            );
            record_failure(
                state,
                delivery.delivery_id,
                delivery.attempt_count,
                None,
                "payload_serialization_failed",
            )
            .await;
            return;
        }
    };

    let timestamp = chrono::Utc::now().timestamp().to_string();
    let signed_content = format!("{timestamp}.{}", String::from_utf8_lossy(&body));

    let mut mac = match HmacSha256::new_from_slice(delivery.secret.as_bytes()) {
        Ok(mac) => mac,
        Err(error) => {
            tracing::error!(?error, "failed to initialize webhook signature");
            record_failure(
                state,
                delivery.delivery_id,
                delivery.attempt_count,
                None,
                "signature_initialization_failed",
            )
            .await;
            return;
        }
    };

    mac.update(signed_content.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());
    let signature_header = format!("t={timestamp},v1={signature}");

    let result = client
        .post(&delivery.url)
        .header("Content-Type", "application/json")
        .header("X-Dodo-Timestamp", &timestamp)
        .header("X-Dodo-Signature", signature_header)
        .body(body)
        .send()
        .await;

    match result {
        Ok(response) if response.status().is_success() => {
            let http_status = i32::from(response.status().as_u16());

            if let Err(error) = sqlx::query(
                r#"
                UPDATE webhook_deliveries
                SET status = 'delivered',
                    last_http_status = $1,
                    last_error = NULL,
                    updated_at = NOW()
                WHERE id = $2
                "#,
            )
            .bind(http_status)
            .bind(delivery.delivery_id)
            .execute(&state.db)
            .await
            {
                tracing::error!(
                    ?error,
                    delivery_id = %delivery.delivery_id,
                    "failed to mark webhook delivered"
                );
            }
        }
        Ok(response) => {
            let status = i32::from(response.status().as_u16());
            record_failure(
                state,
                delivery.delivery_id,
                delivery.attempt_count,
                Some(status),
                &format!("endpoint returned HTTP {status}"),
            )
            .await;
        }
        Err(error) => {
            record_failure(
                state,
                delivery.delivery_id,
                delivery.attempt_count,
                None,
                &format!("webhook request failed: {error}"),
            )
            .await;
        }
    }
}

async fn record_failure(
    state: &AppState,
    delivery_id: Uuid,
    attempt_count: i32,
    http_status: Option<i32>,
    error_message: &str,
) {
    if attempt_count >= MAX_ATTEMPTS {
        if let Err(error) = sqlx::query(
            r#"
            UPDATE webhook_deliveries
            SET status = 'exhausted',
                last_http_status = $1,
                last_error = $2,
                updated_at = NOW()
            WHERE id = $3
            "#,
        )
        .bind(http_status)
        .bind(error_message)
        .bind(delivery_id)
        .execute(&state.db)
        .await
        {
            tracing::error!(?error, "failed to exhaust webhook delivery");
        }

        return;
    }

    // 2, 4, 8, 16... seconds, capped at one hour.
    let exponent = u32::try_from(attempt_count.saturating_sub(1))
        .unwrap_or(0)
        .min(11);
    let delay_seconds = (2_i64.pow(exponent)).min(3600);

    if let Err(error) = sqlx::query(
        r#"
        UPDATE webhook_deliveries
        SET status = 'pending',
            next_attempt_at = NOW() + ($1 * INTERVAL '1 second'),
            last_http_status = $2,
            last_error = $3,
            updated_at = NOW()
        WHERE id = $4
        "#,
    )
    .bind(delay_seconds as f64)
    .bind(http_status)
    .bind(error_message)
    .bind(delivery_id)
    .execute(&state.db)
    .await
    {
        tracing::error!(?error, "failed to schedule webhook retry");
    }
}
