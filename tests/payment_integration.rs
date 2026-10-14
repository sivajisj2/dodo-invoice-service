use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use dodo_invoice_service::{config::Config, http, state::AppState};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::env;
use tower::ServiceExt;

// ── Helpers ──────────────────────────────────────────────────────────────────

async fn setup(max_connections: u32) -> (Router, PgPool) {
    dotenvy::dotenv().ok();

    let database_url =
        env::var("TEST_DATABASE_URL").expect("Set TEST_DATABASE_URL to a dedicated test database");

    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(&database_url)
        .await
        .expect("connect to test database");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("apply migrations");

    let config = Config::from_env().expect("load config");
    let app = http::router(AppState::new(config, pool.clone()));

    (app, pool)
}

async fn send_json(
    app: &Router,
    method: Method,
    uri: &str,
    api_key: Option<&str>,
    idempotency_key: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");

    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    if let Some(key) = idempotency_key {
        builder = builder.header("Idempotency-Key", key);
    }

    let request = builder
        .body(Body::from(body.to_string()))
        .expect("request should build");

    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("router should handle request");

    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("response body should be readable");

    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(&bytes) }));

    (status, value)
}

async fn create_business(app: &Router, name: &str) -> String {
    let (status, body) = send_json(
        app,
        Method::POST,
        "/v1/businesses",
        None,
        None,
        json!({ "name": name }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["api_key"]
        .as_str()
        .expect("api_key in response")
        .to_owned()
}

async fn create_customer(app: &Router, api_key: &str, name: &str) -> String {
    let (status, body) = send_json(
        app,
        Method::POST,
        "/v1/customers",
        Some(api_key),
        None,
        json!({ "name": name }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().expect("id in response").to_owned()
}

async fn create_invoice(
    app: &Router,
    api_key: &str,
    customer_id: &str,
    amount_cents: i64,
) -> String {
    let (status, body) = send_json(
        app,
        Method::POST,
        "/v1/invoices",
        Some(api_key),
        None,
        json!({
            "customer_id": customer_id,
            "line_items": [{ "description": "item", "quantity": 1, "unit_amount_cents": amount_cents }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["invoice"]["total_cents"], amount_cents);
    assert_eq!(body["invoice"]["state"], "open");
    body["invoice"]["id"]
        .as_str()
        .expect("invoice.id in response")
        .to_owned()
}

async fn attempt_count(pool: &PgPool, invoice_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM payment_attempts WHERE invoice_id = $1")
        .bind(uuid::Uuid::parse_str(invoice_id).expect("valid invoice UUID"))
        .fetch_one(pool)
        .await
        .expect("count payment attempts")
}

async fn invoice_state(pool: &PgPool, invoice_id: &str) -> String {
    sqlx::query_scalar("SELECT state FROM invoices WHERE id = $1")
        .bind(uuid::Uuid::parse_str(invoice_id).expect("valid invoice UUID"))
        .fetch_one(pool)
        .await
        .expect("read invoice state")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn successful_payment_retry_reuses_same_attempt() {
    let (app, pool) = setup(5).await;

    let api_key = create_business(&app, "Retry Test Business").await;
    let customer_id = create_customer(&app, &api_key, "Retry Test Customer").await;
    let invoice_id = create_invoice(&app, &api_key, &customer_id, 2500).await;

    let idempotency_key = format!("integration-success-{}", uuid::Uuid::new_v4());
    let pay_uri = format!("/v1/invoices/{invoice_id}/pay");

    let (first_status, first_body) = send_json(
        &app,
        Method::POST,
        &pay_uri,
        Some(&api_key),
        Some(&idempotency_key),
        json!({ "token": "tok_success" }),
    )
    .await;

    assert_eq!(first_status, StatusCode::OK, "{first_body}");
    assert_eq!(first_body["status"], "succeeded");

    let first_attempt_id = first_body["payment_attempt_id"]
        .as_str()
        .expect("payment_attempt_id in response")
        .to_owned();

    // Retry with the same key and identical request.
    let (retry_status, retry_body) = send_json(
        &app,
        Method::POST,
        &pay_uri,
        Some(&api_key),
        Some(&idempotency_key),
        json!({ "token": "tok_success" }),
    )
    .await;

    assert_eq!(retry_status, first_status);
    assert_eq!(retry_body, first_body);
    assert_eq!(
        retry_body["payment_attempt_id"].as_str(),
        Some(first_attempt_id.as_str())
    );

    assert_eq!(attempt_count(&pool, &invoice_id).await, 1);
    assert_eq!(invoice_state(&pool, &invoice_id).await, "paid");

    pool.close().await;
}

#[tokio::test]
async fn reusing_idempotency_key_with_different_request_returns_conflict() {
    let (app, pool) = setup(5).await;

    let api_key = create_business(&app, "Conflict Test Business").await;
    let customer_id = create_customer(&app, &api_key, "Conflict Test Customer").await;
    let invoice_id = create_invoice(&app, &api_key, &customer_id, 2500).await;

    let idempotency_key = format!("integration-conflict-{}", uuid::Uuid::new_v4());
    let pay_uri = format!("/v1/invoices/{invoice_id}/pay");

    let (first_status, first_body) = send_json(
        &app,
        Method::POST,
        &pay_uri,
        Some(&api_key),
        Some(&idempotency_key),
        json!({ "token": "tok_success" }),
    )
    .await;

    assert_eq!(first_status, StatusCode::OK, "{first_body}");
    assert_eq!(first_body["status"], "succeeded");

    // Same key, different token — must conflict.
    let (conflict_status, conflict_body) = send_json(
        &app,
        Method::POST,
        &pay_uri,
        Some(&api_key),
        Some(&idempotency_key),
        json!({ "token": "tok_card_declined" }),
    )
    .await;

    assert_eq!(conflict_status, StatusCode::CONFLICT, "{conflict_body}");
    assert_eq!(attempt_count(&pool, &invoice_id).await, 1);

    pool.close().await;
}

#[tokio::test]
async fn concurrent_payments_create_at_most_one_attempt() {
    let (app, pool) = setup(10).await;

    let api_key = create_business(&app, "Concurrent Payment Test").await;
    let customer_id = create_customer(&app, &api_key, "Concurrent Test Customer").await;
    let invoice_id = create_invoice(&app, &api_key, &customer_id, 2500).await;

    let pay_uri = format!("/v1/invoices/{invoice_id}/pay");
    let idempotency_key = format!("concurrent-payment-{}", uuid::Uuid::new_v4());

    // Fire five requests with the same invoice and idempotency key.
    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let app = app.clone();
            let uri = pay_uri.clone();
            let key = idempotency_key.clone();
            let api_key = api_key.clone();
            tokio::spawn(async move {
                send_json(
                    &app,
                    Method::POST,
                    &uri,
                    Some(&api_key),
                    Some(&key),
                    json!({ "token": "tok_success" }),
                )
                .await
            })
        })
        .collect();

    let mut successful_or_accepted = 0;

    for task in tasks {
        let (status, body) = task.await.expect("task should finish");
        assert!(
            status == StatusCode::OK || status == StatusCode::ACCEPTED,
            "unexpected response: status={status}, body={body}"
        );
        successful_or_accepted += 1;
    }

    assert_eq!(successful_or_accepted, 5);
    assert_eq!(
        attempt_count(&pool, &invoice_id).await,
        1,
        "five concurrent requests must not create duplicate attempts"
    );

    let succeeded: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM payment_attempts WHERE invoice_id = $1 AND status = 'succeeded'",
    )
    .bind(uuid::Uuid::parse_str(&invoice_id).expect("valid invoice UUID"))
    .fetch_one(&pool)
    .await
    .expect("count succeeded attempts");

    assert_eq!(succeeded, 1);

    pool.close().await;
}

#[tokio::test]
async fn concurrent_payments_with_different_keys_do_not_duplicate_attempts() {
    let (app, pool) = setup(10).await;

    let api_key = create_business(&app, "Different Keys Concurrency Test").await;
    let customer_id = create_customer(&app, &api_key, "Concurrency Customer").await;
    let invoice_id = create_invoice(&app, &api_key, &customer_id, 2500).await;

    let pay_uri = format!("/v1/invoices/{invoice_id}/pay");

    // Each request gets its own idempotency key.
    let tasks: Vec<_> = (0..5)
        .map(|i| {
            let app = app.clone();
            let uri = pay_uri.clone();
            let api_key = api_key.clone();
            let idempotency_key = format!("different-key-{}-{}", uuid::Uuid::new_v4(), i);
            tokio::spawn(async move {
                send_json(
                    &app,
                    Method::POST,
                    &uri,
                    Some(&api_key),
                    Some(&idempotency_key),
                    json!({ "token": "tok_success" }),
                )
                .await
            })
        })
        .collect();

    let mut accepted_count = 0;
    let mut conflict_count = 0;

    for task in tasks {
        let (status, body) = task.await.expect("task should finish");
        match status {
            StatusCode::OK => {
                accepted_count += 1;
                assert_eq!(body["status"], "succeeded", "{body}");
            }
            StatusCode::ACCEPTED => {
                accepted_count += 1;
                assert!(
                    body["status"] == "pending" || body["status"] == "unknown",
                    "unexpected accepted response: {body}"
                );
            }
            StatusCode::CONFLICT => conflict_count += 1,
            _ => panic!("unexpected response: status={status}, body={body}"),
        }
    }

    assert!(
        accepted_count >= 1,
        "at least one request should be accepted"
    );
    assert_eq!(
        accepted_count + conflict_count,
        5,
        "all five requests should have a recognized response"
    );

    assert_eq!(
        attempt_count(&pool, &invoice_id).await,
        1,
        "different idempotency keys must not create duplicate attempts"
    );

    let succeeded: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM payment_attempts WHERE invoice_id = $1 AND status = 'succeeded'",
    )
    .bind(uuid::Uuid::parse_str(&invoice_id).expect("valid invoice UUID"))
    .fetch_one(&pool)
    .await
    .expect("count succeeded attempts");

    assert!(
        succeeded <= 1,
        "must not have multiple successful payment attempts"
    );

    pool.close().await;
}
