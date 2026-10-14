use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, time::sleep};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use dodo_invoice_service::config::Config;

#[derive(Debug, Deserialize)]
struct PaymentRequest {
    amount_cents: i64,
    token: String,
}

#[derive(Clone)]
struct MockPspState {
    payments: Arc<Mutex<HashMap<String, CachedPayment>>>,
}

#[derive(Clone)]
struct CachedPayment {
    request_fingerprint: String,
    status: StatusCode,
    body: Value,
}

impl MockPspState {
    fn new() -> Self {
        Self {
            payments: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    let state = MockPspState::new();

    let app = Router::new()
        .route("/health", get(get_health))
        .route("/payments", post(create_payment))
        .route("/payments/{idempotency_key}", get(get_payment))
        .with_state(state);

    let address = config.mock_psp_address.clone();
    let listener = TcpListener::bind(&address).await?;

    info!(address = %address, "mock PSP started");

    axum::serve(listener, app).await?;

    Ok(())
}

async fn get_health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "mock-psp"
    }))
}
async fn get_payment(
    State(state): State<MockPspState>,
    axum::extract::Path(idempotency_key): axum::extract::Path<String>,
) -> (StatusCode, Json<Value>) {
    let payments = state.payments.lock().await;

    match payments.get(&idempotency_key) {
        Some(payment) => (payment.status, Json(payment.body.clone())),

        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "status": "not_found",
                "message": "No payment result is available for this key"
            })),
        ),
    }
}

async fn create_payment(
    State(state): State<MockPspState>,
    headers: HeaderMap,
    Json(request): Json<PaymentRequest>,
) -> (StatusCode, Json<Value>) {
    let Some(key) = headers
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 200)
        .map(str::to_owned)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "A valid Idempotency-Key header is required"
            })),
        );
    };

    if request.amount_cents <= 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "failed",
                "error": "amount_cents must be greater than zero"
            })),
        );
    }

    if !matches!(
        request.token.as_str(),
        "tok_success"
            | "tok_insufficient_funds"
            | "tok_card_declined"
            | "tok_timeout"
            | "tok_network_error"
    ) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "failed",
                "error": "unsupported payment token"
            })),
        );
    }
    let fingerprint = format!("{}:{}", request.amount_cents, request.token);

    let mut payments = state.payments.lock().await;

    if let Some(cached) = payments.get(&key) {
        if cached.request_fingerprint != fingerprint {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "idempotency_key_reused",
                    "message": "The key was already used for a different request"
                })),
            );
        }

        info!(idempotency_key = %key, "replaying cached PSP result");

        return (cached.status, Json(cached.body.clone()));
    }

    // Reserve the key before processing, then release the mutex so lookups
    // and duplicate requests can observe the in-flight payment instead of
    // blocking behind the simulated delay.
    payments.insert(
        key.clone(),
        CachedPayment {
            request_fingerprint: fingerprint.clone(),
            status: StatusCode::ACCEPTED,
            body: json!({
                "status": "processing"
            }),
        },
    );

    drop(payments);

    // Process in a detached task. If the caller disconnects — as the invoice
    // service does when its own request timeout fires — axum cancels this
    // handler, but the charge must still complete the way a real provider's
    // would. That outcome is what reconciliation later recovers.
    let processing = tokio::spawn(async move {
        let (status, body) = match request.token.as_str() {
            "tok_success" => {
                sleep(Duration::from_millis(100)).await;

                (
                    StatusCode::OK,
                    json!({
                        "status": "succeeded",
                        "psp_reference": uuid::Uuid::new_v4().to_string()
                    }),
                )
            }

            "tok_insufficient_funds" => {
                sleep(Duration::from_millis(100)).await;

                (
                    StatusCode::OK,
                    json!({
                        "status": "failed",
                        "failure_code": "insufficient_funds"
                    }),
                )
            }

            "tok_card_declined" => {
                sleep(Duration::from_millis(100)).await;

                (
                    StatusCode::OK,
                    json!({
                        "status": "failed",
                        "failure_code": "card_declined"
                    }),
                )
            }

            "tok_timeout" => {
                warn!("simulating a 30-second PSP delay");
                sleep(Duration::from_secs(30)).await;

                (
                    StatusCode::OK,
                    json!({
                        "status": "succeeded",
                        "psp_reference": uuid::Uuid::new_v4().to_string()
                    }),
                )
            }

            "tok_network_error" => {
                warn!("simulating PSP network failure");

                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({
                        "status": "error",
                        "error": "simulated_network_error"
                    }),
                )
            }

            // Unsupported tokens are rejected before the key is reserved.
            token => unreachable!("unvalidated payment token: {token}"),
        };

        // Replace the reservation with the final outcome.
        let mut payments = state.payments.lock().await;

        payments.insert(
            key.clone(),
            CachedPayment {
                request_fingerprint: fingerprint,
                status,
                body: body.clone(),
            },
        );

        info!(idempotency_key = %key, "saved PSP result");

        (status, body)
    });

    match processing.await {
        Ok((status, body)) => (status, Json(body)),
        Err(error) => {
            warn!(?error, "PSP processing task did not complete");

            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "error": "psp_processing_failed"
                })),
            )
        }
    }
}
