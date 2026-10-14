use axum::{
    Json,
    extract::{Extension, Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use uuid::Uuid;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct PayInvoiceRequest {
    pub token: String,
}

#[derive(Debug, FromRow)]
struct IdempotencyRecord {
    request_hash: String,
    payment_attempt_id: Option<Uuid>,
    response_status: Option<i32>,
    response_body: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct PspResponse {
    status: String,
    psp_reference: Option<String>,
    failure_code: Option<String>,
}

fn hash_request(invoice_id: Uuid, token: &str) -> String {
    let input = format!("{invoice_id}:{token}");
    hex::encode(Sha256::digest(input.as_bytes()))
}

fn json_response(status: StatusCode, body: Value) -> (StatusCode, Json<Value>) {
    (status, Json(body))
}

pub async fn pay_invoice(
    State(state): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Path(invoice_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<PayInvoiceRequest>,
) -> (StatusCode, Json<Value>) {
    // 1. Validate the idempotency key.
    let Some(idempotency_key) = headers
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 200)
    else {
        return json_response(
            StatusCode::BAD_REQUEST,
            crate::error::error_body_with(
                "idempotency_key_required",
                "A valid Idempotency-Key header is required",
            ),
        );
    };

    if payload.token.trim().is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            crate::error::error_body_with("token_required", "A card token is required"),
        );
    }

    let request_hash = hash_request(invoice_id, &payload.token);

    // 2. Reserve the idempotency key.
    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::error!(?error, "failed to begin payment transaction");
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::error::error_body("internal_error"),
            );
        }
    };

    let reservation = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO idempotency_records (
            id, business_id, idempotency_key, request_hash
        )
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (business_id, idempotency_key) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(business_id)
    .bind(idempotency_key)
    .bind(&request_hash)
    .fetch_optional(&mut *tx)
    .await;

    let reservation_id = match reservation {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(?error, "failed to reserve idempotency key");
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::error::error_body("internal_error"),
            );
        }
    };

    // If the key already exists, return its stored result or report
    // that the original request is still being processed.
    if reservation_id.is_none() {
        let existing = sqlx::query_as::<_, IdempotencyRecord>(
            r#"
            SELECT request_hash, payment_attempt_id,
                   response_status, response_body
            FROM idempotency_records
            WHERE business_id = $1 AND idempotency_key = $2
            "#,
        )
        .bind(business_id)
        .bind(idempotency_key)
        .fetch_optional(&mut *tx)
        .await;

        let existing = match existing {
            Ok(Some(record)) => record,
            Ok(None) => {
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    crate::error::error_body("idempotency_record_missing"),
                );
            }
            Err(error) => {
                tracing::error!(?error, "failed to load idempotency record");
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    crate::error::error_body("internal_error"),
                );
            }
        };

        if existing.request_hash != request_hash {
            return json_response(
                StatusCode::CONFLICT,
                json!({
                    "error": "idempotency_key_reused",
                    "message": "Use a new key for a different payment request"
                }),
            );
        }

        if let (Some(status), Some(body)) = (existing.response_status, existing.response_body) {
            let status = u16::try_from(status)
                .ok()
                .and_then(|value| StatusCode::from_u16(value).ok())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

            return json_response(status, body);
        }

        return json_response(
            StatusCode::ACCEPTED,
            json!({
                "status": "pending",
                "payment_attempt_id": existing.payment_attempt_id,
                "message": "The original payment request is still being processed"
            }),
        );
    }

    // 3. Lock the invoice row. Different keys cannot reserve concurrent
    // payments for the same invoice without observing this lock.
    let invoice = sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT state, total_cents
        FROM invoices
        WHERE id = $1 AND business_id = $2
        FOR UPDATE
        "#,
    )
    .bind(invoice_id)
    .bind(business_id)
    .fetch_optional(&mut *tx)
    .await;

    let (invoice_state, amount_cents) = match invoice {
        Ok(Some(invoice)) => invoice,
        Ok(None) => {
            return json_response(
                StatusCode::NOT_FOUND,
                crate::error::error_body("invoice_not_found"),
            );
        }
        Err(error) => {
            tracing::error!(?error, "failed to lock invoice");
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::error::error_body("internal_error"),
            );
        }
    };

    if invoice_state != "open" {
        return json_response(
            StatusCode::CONFLICT,
            json!({
                "error": "invoice_not_payable",
                "state": invoice_state
            }),
        );
    }

    // A previous uncertain or pending attempt must not be charged again
    // automatically. It requires reconciliation.
    let existing_attempt: Result<bool, sqlx::Error> = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM payment_attempts
            WHERE invoice_id = $1
              AND status IN ('pending', 'unknown')
        )
        "#,
    )
    .bind(invoice_id)
    .fetch_one(&mut *tx)
    .await;

    match existing_attempt {
        Ok(true) => {
            return json_response(
                StatusCode::CONFLICT,
                json!({
                    "error": "payment_requires_reconciliation",
                    "message": "An existing payment attempt is pending or has an unknown outcome"
                }),
            );
        }
        Ok(false) => {}
        Err(error) => {
            tracing::error!(?error, "failed to check previous payment attempts");
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::error::error_body("internal_error"),
            );
        }
    }

    // 4. Persist the attempt before contacting the PSP.
    let attempt_id = Uuid::new_v4();
    let provider_key = format!("dodo-{attempt_id}");

    let create_attempt = sqlx::query(
        r#"
        INSERT INTO payment_attempts (
            id, invoice_id, status, amount_cents,
            idempotency_key, request_hash, provider_idempotency_key
        )
        VALUES ($1, $2, 'pending', $3, $4, $5, $6)
        "#,
    )
    .bind(attempt_id)
    .bind(invoice_id)
    .bind(amount_cents)
    .bind(idempotency_key)
    .bind(&request_hash)
    .bind(&provider_key)
    .execute(&mut *tx)
    .await;

    if let Err(error) = create_attempt {
        tracing::error!(?error, "failed to create payment attempt");
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::error::error_body("internal_error"),
        );
    }

    let link_attempt = sqlx::query(
        r#"
        UPDATE idempotency_records
        SET payment_attempt_id = $1, updated_at = NOW()
        WHERE business_id = $2 AND idempotency_key = $3
        "#,
    )
    .bind(attempt_id)
    .bind(business_id)
    .bind(idempotency_key)
    .execute(&mut *tx)
    .await;

    if let Err(error) = link_attempt {
        tracing::error!(?error, "failed to link idempotency record");
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::error::error_body("internal_error"),
        );
    }

    if let Err(error) = tx.commit().await {
        tracing::error!(?error, "failed to commit pending payment");
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::error::error_body("internal_error"),
        );
    }

    // 5. Call the external PSP only after the database commit.
    let psp_url =
        std::env::var("MOCK_PSP_URL").unwrap_or_else(|_| "http://127.0.0.1:9000".to_string());

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(?error, "failed to create PSP HTTP client");
            return finalize_payment(
                &state,
                business_id,
                invoice_id,
                attempt_id,
                idempotency_key,
                "unknown",
                None,
                Some("psp_client_error".to_string()),
            )
            .await;
        }
    };

    let psp_result = client
        .post(format!("{}/payments", psp_url.trim_end_matches('/')))
        .header("Idempotency-Key", &provider_key)
        .json(&json!({
            "amount_cents": amount_cents,
            "token": payload.token
        }))
        .send()
        .await;

    let psp_response = match psp_result {
        Ok(response) if response.status().is_success() => {
            match response.json::<PspResponse>().await {
                Ok(result) => Some(result),
                Err(error) => {
                    tracing::warn!(?error, "PSP returned an unreadable response");
                    None
                }
            }
        }
        Ok(response) => {
            tracing::warn!(
                status = %response.status(),
                "PSP returned a non-success HTTP status"
            );
            None
        }
        Err(error) => {
            tracing::warn!(?error, "PSP request failed or timed out");
            None
        }
    };

    match psp_response {
        Some(response) if response.status == "succeeded" => {
            finalize_payment(
                &state,
                business_id,
                invoice_id,
                attempt_id,
                idempotency_key,
                "succeeded",
                response.psp_reference,
                None,
            )
            .await
        }
        Some(response) if response.status == "failed" => {
            finalize_payment(
                &state,
                business_id,
                invoice_id,
                attempt_id,
                idempotency_key,
                "failed",
                None,
                Some(
                    response
                        .failure_code
                        .unwrap_or_else(|| "payment_failed".to_string()),
                ),
            )
            .await
        }
        _ => {
            finalize_payment(
                &state,
                business_id,
                invoice_id,
                attempt_id,
                idempotency_key,
                "unknown",
                None,
                Some("psp_outcome_unknown".to_string()),
            )
            .await
        }
    }
}

pub async fn reconcile_payment(
    State(state): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Path(attempt_id): Path<Uuid>,
) -> (StatusCode, Json<Value>) {
    let attempt = sqlx::query_as::<_, (Uuid, Uuid, String, String, String)>(
        r#"
        SELECT
            pa.id,
            pa.invoice_id,
            pa.status,
            pa.idempotency_key,
            pa.provider_idempotency_key
        FROM payment_attempts pa
        JOIN invoices i ON i.id = pa.invoice_id
        WHERE pa.id = $1 AND i.business_id = $2
        "#,
    )
    .bind(attempt_id)
    .bind(business_id)
    .fetch_optional(&state.db)
    .await;

    let (attempt_id, invoice_id, status, idempotency_key, provider_key) = match attempt {
        Ok(Some(attempt)) => attempt,
        Ok(None) => {
            return json_response(
                StatusCode::NOT_FOUND,
                crate::error::error_body("payment_attempt_not_found"),
            );
        }
        Err(error) => {
            tracing::error!(?error, "failed to load payment attempt");

            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::error::error_body("internal_error"),
            );
        }
    };

    if status == "succeeded" || status == "failed" {
        return json_response(
            StatusCode::OK,
            json!({
                "payment_attempt_id": attempt_id,
                "invoice_id": invoice_id,
                "status": status,
                "message": "Payment already has a final result"
            }),
        );
    }

    let psp_url =
        std::env::var("MOCK_PSP_URL").unwrap_or_else(|_| "http://127.0.0.1:9000".to_string());

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(?error, "failed to create PSP HTTP client");

            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::error::error_body("internal_error"),
            );
        }
    };

    let response = client
        .get(format!(
            "{}/payments/{}",
            psp_url.trim_end_matches('/'),
            provider_key
        ))
        .send()
        .await;

    let response = match response {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            tracing::warn!(
                status = %response.status(),
                "PSP has no usable reconciliation result"
            );

            return json_response(
                StatusCode::ACCEPTED,
                json!({
                    "payment_attempt_id": attempt_id,
                    "invoice_id": invoice_id,
                    "status": "unknown",
                    "message": "Payment outcome is not confirmed yet"
                }),
            );
        }
        Err(error) => {
            tracing::warn!(?error, "PSP reconciliation request failed");

            return json_response(
                StatusCode::ACCEPTED,
                json!({
                    "payment_attempt_id": attempt_id,
                    "invoice_id": invoice_id,
                    "status": "unknown",
                    "message": "Payment outcome is not confirmed yet"
                }),
            );
        }
    };

    let psp_result = match response.json::<PspResponse>().await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(?error, "PSP reconciliation response was invalid");

            return json_response(
                StatusCode::ACCEPTED,
                json!({
                    "payment_attempt_id": attempt_id,
                    "invoice_id": invoice_id,
                    "status": "unknown",
                    "message": "Payment outcome is not confirmed yet"
                }),
            );
        }
    };

    match psp_result.status.as_str() {
        "succeeded" => {
            finalize_payment(
                &state,
                business_id,
                invoice_id,
                attempt_id,
                &idempotency_key,
                "succeeded",
                psp_result.psp_reference,
                None,
            )
            .await
        }
        "failed" => {
            finalize_payment(
                &state,
                business_id,
                invoice_id,
                attempt_id,
                &idempotency_key,
                "failed",
                None,
                Some(
                    psp_result
                        .failure_code
                        .unwrap_or_else(|| "payment_failed".to_string()),
                ),
            )
            .await
        }
        _ => json_response(
            StatusCode::ACCEPTED,
            json!({
                "payment_attempt_id": attempt_id,
                "invoice_id": invoice_id,
                "status": "unknown",
                "message": "Payment is still processing"
            }),
        ),
    }
}

async fn finalize_payment(
    state: &AppState,
    business_id: Uuid,
    invoice_id: Uuid,
    attempt_id: Uuid,
    idempotency_key: &str,
    outcome: &str,
    psp_reference: Option<String>,
    failure_code: Option<String>,
) -> (StatusCode, Json<Value>) {
    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::error!(?error, "failed to begin payment finalization");
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::error::error_body("payment_finalization_failed"),
            );
        }
    };

    let (http_status, body) = match outcome {
        "succeeded" => {
            let update_invoice = sqlx::query(
                r#"
                UPDATE invoices
                SET state = 'paid', updated_at = NOW()
                WHERE id = $1 AND business_id = $2 AND state = 'open'
                "#,
            )
            .bind(invoice_id)
            .bind(business_id)
            .execute(&mut *tx)
            .await;

            match update_invoice {
                Ok(result) if result.rows_affected() == 1 => (
                    StatusCode::OK,
                    json!({
                        "payment_attempt_id": attempt_id,
                        "invoice_id": invoice_id,
                        "status": "succeeded",
                        "psp_reference": psp_reference
                    }),
                ),
                Ok(_) => {
                    return json_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        crate::error::error_body("invoice_state_changed_during_payment"),
                    );
                }
                Err(error) => {
                    tracing::error!(?error, "failed to mark invoice paid");
                    return json_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        crate::error::error_body("payment_finalization_failed"),
                    );
                }
            }
        }
        "failed" => (
            StatusCode::OK,
            json!({
                "payment_attempt_id": attempt_id,
                "invoice_id": invoice_id,
                "status": "failed",
                "failure_code": failure_code
            }),
        ),
        _ => (
            StatusCode::ACCEPTED,
            json!({
                "payment_attempt_id": attempt_id,
                "invoice_id": invoice_id,
                "status": "unknown",
                "failure_code": failure_code,
                "message": "Do not retry with a new key until the payment is reconciled"
            }),
        ),
    };

    let attempt_status = match outcome {
        "succeeded" => "succeeded",
        "failed" => "failed",
        _ => "unknown",
    };

    let update_attempt = sqlx::query(
        r#"
        UPDATE payment_attempts
        SET status = $1,
            psp_reference = $2,
            failure_code = $3,
            updated_at = NOW(),
            completed_at = NOW()
        WHERE id = $4
        "#,
    )
    .bind(attempt_status)
    .bind(&psp_reference)
    .bind(&failure_code)
    .bind(attempt_id)
    .execute(&mut *tx)
    .await;

    if let Err(error) = update_attempt {
        tracing::error!(?error, "failed to update payment attempt");
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::error::error_body("payment_finalization_failed"),
        );
    }

    let update_idempotency = sqlx::query(
        r#"
        UPDATE idempotency_records
        SET response_status = $1,
            response_body = $2,
            updated_at = NOW()
        WHERE business_id = $3 AND idempotency_key = $4
        "#,
    )
    .bind(i32::from(http_status.as_u16()))
    .bind(&body)
    .bind(business_id)
    .bind(idempotency_key)
    .execute(&mut *tx)
    .await;

    if let Err(error) = update_idempotency {
        tracing::error!(?error, "failed to save idempotent response");
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::error::error_body("payment_finalization_failed"),
        );
    }

    // Create a webhook event only when the payment has a confirmed outcome.
    // "unknown" is deliberately excluded because reconciliation is still needed.
    let event_type = match outcome {
        "succeeded" => Some("invoice.paid"),
        "failed" => Some("invoice.payment_failed"),
        _ => None,
    };

    if let Some(event_type) = event_type {
        let event_id = Uuid::new_v4();

        let payload = json!({
            "id": event_id,
            "type": event_type,
            "data": {
                "invoice_id": invoice_id,
                "payment_attempt_id": attempt_id,
                "status": outcome,
                "psp_reference": psp_reference,
                "failure_code": failure_code
            }
        });

        let create_event = sqlx::query(
            r#"
            INSERT INTO webhook_events (
                id, business_id, event_type, payload
            )
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(event_id)
        .bind(business_id)
        .bind(event_type)
        .bind(&payload)
        .execute(&mut *tx)
        .await;

        if let Err(error) = create_event {
            tracing::error!(?error, "failed to create webhook event");

            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::error::error_body("payment_finalization_failed"),
            );
        }

        // Create one pending delivery per enabled endpoint belonging
        // to this business. The unique constraint prevents duplicates.
        let create_deliveries = sqlx::query(
            r#"
            INSERT INTO webhook_deliveries (
                id, event_id, endpoint_id, status, next_attempt_at
            )
            SELECT
                gen_random_uuid(),
                $1,
                endpoint.id,
                'pending',
                NOW()
            FROM webhook_endpoints AS endpoint
            WHERE endpoint.business_id = $2
              AND endpoint.enabled = TRUE
            ON CONFLICT (event_id, endpoint_id) DO NOTHING
            "#,
        )
        .bind(event_id)
        .bind(business_id)
        .execute(&mut *tx)
        .await;

        if let Err(error) = create_deliveries {
            tracing::error!(?error, "failed to create webhook deliveries");

            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::error::error_body("payment_finalization_failed"),
            );
        }
    }

    if let Err(error) = tx.commit().await {
        tracing::error!(?error, "failed to commit payment result");
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::error::error_body("payment_finalization_failed"),
        );
    }

    json_response(http_status, body)
}
