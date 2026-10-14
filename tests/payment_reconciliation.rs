use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use dodo_invoice_service::{config::Config, http, state::AppState};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{env, time::Duration};
use tower::ServiceExt;
use uuid::Uuid;

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

async fn business_id(app: &Router, api_key: &str) -> Uuid {
    let (status, body) =
        send_json(app, Method::GET, "/v1/me", Some(api_key), None, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Uuid::parse_str(
        body["business_id"]
            .as_str()
            .expect("business_id in response"),
    )
    .expect("valid business UUID")
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

async fn create_invoice(app: &Router, api_key: &str, customer_id: &str, amount_cents: i64) -> Uuid {
    let (status, body) = send_json(
        app,
        Method::POST,
        "/v1/invoices",
        Some(api_key),
        None,
        json!({
            "customer_id": customer_id,
            "line_items": [{
                "description": "reconciliation item",
                "quantity": 1,
                "unit_amount_cents": amount_cents
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Uuid::parse_str(
        body["invoice"]["id"]
            .as_str()
            .expect("invoice.id in response"),
    )
    .expect("valid invoice UUID")
}

fn psp_base_url() -> String {
    env::var("MOCK_PSP_URL").unwrap_or_else(|_| "http://127.0.0.1:9000".to_string())
}

/// Drives a payment straight through the mock PSP so a known outcome is cached
/// under `provider_key`, exactly as a real provider would hold it.
async fn seed_psp_payment(provider_key: &str, token: &str, amount_cents: i64) -> Value {
    let response = reqwest::Client::new()
        .post(format!("{}/payments", psp_base_url().trim_end_matches('/')))
        .header("Idempotency-Key", provider_key)
        .json(&json!({ "amount_cents": amount_cents, "token": token }))
        .send()
        .await
        .expect("mock PSP must be running — start it with `cargo run --bin mock-psp`");

    response.json().await.expect("PSP response should be JSON")
}

/// Represents a payment the service committed but never got a confirmed
/// outcome for, which is the only state reconciliation acts on.
async fn insert_attempt(
    pool: &PgPool,
    business_id: Uuid,
    invoice_id: Uuid,
    provider_key: &str,
    amount_cents: i64,
    status: &str,
) -> Uuid {
    let attempt_id = Uuid::new_v4();
    let idempotency_key = format!("reconcile-seed-{attempt_id}");
    let request_hash = format!("hash-{attempt_id}");

    sqlx::query(
        r#"
        INSERT INTO payment_attempts (
            id, invoice_id, status, amount_cents,
            idempotency_key, request_hash, provider_idempotency_key
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(attempt_id)
    .bind(invoice_id)
    .bind(status)
    .bind(amount_cents)
    .bind(&idempotency_key)
    .bind(&request_hash)
    .bind(provider_key)
    .execute(pool)
    .await
    .expect("insert payment attempt");

    sqlx::query(
        r#"
        INSERT INTO idempotency_records (
            id, business_id, idempotency_key, request_hash, payment_attempt_id
        )
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(business_id)
    .bind(&idempotency_key)
    .bind(&request_hash)
    .bind(attempt_id)
    .execute(pool)
    .await
    .expect("insert idempotency record");

    attempt_id
}

async fn reconcile(app: &Router, api_key: &str, attempt_id: Uuid) -> (StatusCode, Value) {
    send_json(
        app,
        Method::POST,
        &format!("/v1/payment-attempts/{attempt_id}/reconcile"),
        Some(api_key),
        None,
        json!({}),
    )
    .await
}

async fn attempt_status(pool: &PgPool, attempt_id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM payment_attempts WHERE id = $1")
        .bind(attempt_id)
        .fetch_one(pool)
        .await
        .expect("read attempt status")
}

async fn attempt_count(pool: &PgPool, invoice_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM payment_attempts WHERE invoice_id = $1")
        .bind(invoice_id)
        .fetch_one(pool)
        .await
        .expect("count payment attempts")
}

async fn invoice_state(pool: &PgPool, invoice_id: Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM invoices WHERE id = $1")
        .bind(invoice_id)
        .fetch_one(pool)
        .await
        .expect("read invoice state")
}

struct Fixture {
    app: Router,
    pool: PgPool,
    api_key: String,
    business_id: Uuid,
    invoice_id: Uuid,
}

async fn fixture(name: &str) -> Fixture {
    let (app, pool) = setup(5).await;
    let api_key = create_business(&app, name).await;
    let business_id = business_id(&app, &api_key).await;
    let customer_id = create_customer(&app, &api_key, &format!("{name} Customer")).await;
    let invoice_id = create_invoice(&app, &api_key, &customer_id, 2500).await;

    Fixture {
        app,
        pool,
        api_key,
        business_id,
        invoice_id,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn reconciling_confirmed_success_marks_attempt_succeeded_and_invoice_paid() {
    let f = fixture("Reconcile Success Test").await;

    let provider_key = format!("dodo-reconcile-ok-{}", Uuid::new_v4());
    let seeded = seed_psp_payment(&provider_key, "tok_success", 2500).await;
    assert_eq!(seeded["status"], "succeeded", "{seeded}");

    let attempt_id = insert_attempt(
        &f.pool,
        f.business_id,
        f.invoice_id,
        &provider_key,
        2500,
        "unknown",
    )
    .await;

    let (status, body) = reconcile(&f.app, &f.api_key, attempt_id).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "succeeded");
    assert_eq!(body["psp_reference"], seeded["psp_reference"]);

    assert_eq!(attempt_status(&f.pool, attempt_id).await, "succeeded");
    assert_eq!(invoice_state(&f.pool, f.invoice_id).await, "paid");
    assert_eq!(
        attempt_count(&f.pool, f.invoice_id).await,
        1,
        "reconciliation must not create a second charge"
    );

    f.pool.close().await;
}

#[tokio::test]
async fn reconciling_confirmed_failure_marks_attempt_failed_and_leaves_invoice_open() {
    let f = fixture("Reconcile Failure Test").await;

    let provider_key = format!("dodo-reconcile-declined-{}", Uuid::new_v4());
    let seeded = seed_psp_payment(&provider_key, "tok_card_declined", 2500).await;
    assert_eq!(seeded["status"], "failed", "{seeded}");

    let attempt_id = insert_attempt(
        &f.pool,
        f.business_id,
        f.invoice_id,
        &provider_key,
        2500,
        "unknown",
    )
    .await;

    let (status, body) = reconcile(&f.app, &f.api_key, attempt_id).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "failed");
    assert_eq!(body["failure_code"], "card_declined");

    assert_eq!(attempt_status(&f.pool, attempt_id).await, "failed");
    assert_eq!(
        invoice_state(&f.pool, f.invoice_id).await,
        "open",
        "a failed payment must not mark the invoice paid"
    );
    assert_eq!(attempt_count(&f.pool, f.invoice_id).await, 1);

    f.pool.close().await;
}

#[tokio::test]
async fn reconciling_payment_the_psp_never_saw_leaves_it_unresolved() {
    let f = fixture("Reconcile Unknown Test").await;

    // Never seeded, so the PSP has no record of this key.
    let provider_key = format!("dodo-never-sent-{}", Uuid::new_v4());

    let attempt_id = insert_attempt(
        &f.pool,
        f.business_id,
        f.invoice_id,
        &provider_key,
        2500,
        "unknown",
    )
    .await;

    let (status, body) = reconcile(&f.app, &f.api_key, attempt_id).await;

    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["status"], "unknown");

    assert_eq!(
        attempt_status(&f.pool, attempt_id).await,
        "unknown",
        "an unconfirmed outcome must stay unresolved"
    );
    assert_eq!(invoice_state(&f.pool, f.invoice_id).await, "open");
    assert_eq!(
        attempt_count(&f.pool, f.invoice_id).await,
        1,
        "an unknown outcome must never trigger a second charge"
    );

    f.pool.close().await;
}

#[tokio::test]
async fn reconciling_an_in_flight_payment_reports_still_processing() {
    let f = fixture("Reconcile In Flight Test").await;

    let provider_key = format!("dodo-in-flight-{}", Uuid::new_v4());

    // Start a 30-second PSP payment and deliberately do not await it.
    let in_flight_key = provider_key.clone();
    tokio::spawn(async move {
        seed_psp_payment(&in_flight_key, "tok_timeout", 2500).await;
    });

    // Let the PSP reserve the key before looking it up. This only returns
    // promptly because the PSP releases its mutex while processing.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let attempt_id = insert_attempt(
        &f.pool,
        f.business_id,
        f.invoice_id,
        &provider_key,
        2500,
        "unknown",
    )
    .await;

    let (status, body) = reconcile(&f.app, &f.api_key, attempt_id).await;

    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["status"], "unknown");
    assert_eq!(body["message"], "Payment is still processing");

    assert_eq!(attempt_status(&f.pool, attempt_id).await, "unknown");
    assert_eq!(invoice_state(&f.pool, f.invoice_id).await, "open");

    f.pool.close().await;
}

#[tokio::test]
async fn reconciling_an_already_final_attempt_changes_nothing() {
    let f = fixture("Reconcile Final Test").await;

    let provider_key = format!("dodo-already-final-{}", Uuid::new_v4());
    let attempt_id = insert_attempt(
        &f.pool,
        f.business_id,
        f.invoice_id,
        &provider_key,
        2500,
        "failed",
    )
    .await;

    let (status, body) = reconcile(&f.app, &f.api_key, attempt_id).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "failed");
    assert_eq!(body["message"], "Payment already has a final result");

    assert_eq!(attempt_status(&f.pool, attempt_id).await, "failed");
    assert_eq!(invoice_state(&f.pool, f.invoice_id).await, "open");

    f.pool.close().await;
}

#[tokio::test]
async fn reconciliation_is_scoped_to_the_owning_business() {
    let f = fixture("Reconcile Owner Test").await;

    let provider_key = format!("dodo-cross-tenant-{}", Uuid::new_v4());
    seed_psp_payment(&provider_key, "tok_success", 2500).await;

    let attempt_id = insert_attempt(
        &f.pool,
        f.business_id,
        f.invoice_id,
        &provider_key,
        2500,
        "unknown",
    )
    .await;

    // A second business must not be able to reconcile the first one's attempt.
    let other_api_key = create_business(&f.app, "Reconcile Intruder").await;
    let (status, body) = reconcile(&f.app, &other_api_key, attempt_id).await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "payment_attempt_not_found");

    assert_eq!(attempt_status(&f.pool, attempt_id).await, "unknown");
    assert_eq!(invoice_state(&f.pool, f.invoice_id).await, "open");

    f.pool.close().await;
}

/// The full recovery path from the original request through to final database
/// state: the service gives up after its own 5s timeout while the PSP keeps
/// processing for 30s, and only reconciliation resolves the outcome.
#[tokio::test]
async fn timeout_recovery_resolves_to_succeeded_without_a_second_charge() {
    let f = fixture("Reconcile Timeout Recovery Test").await;

    let idempotency_key = format!("reconcile-timeout-{}", Uuid::new_v4());

    let (status, body) = send_json(
        &f.app,
        Method::POST,
        &format!("/v1/invoices/{}/pay", f.invoice_id),
        Some(&f.api_key),
        Some(&idempotency_key),
        json!({ "token": "tok_timeout" }),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["status"], "unknown");

    let attempt_id = Uuid::parse_str(
        body["payment_attempt_id"]
            .as_str()
            .expect("payment_attempt_id in response"),
    )
    .expect("valid attempt UUID");

    assert_eq!(attempt_status(&f.pool, attempt_id).await, "unknown");
    assert_eq!(invoice_state(&f.pool, f.invoice_id).await, "open");

    // Wait out the remainder of the PSP's 30-second processing window.
    tokio::time::sleep(Duration::from_secs(28)).await;

    let (status, body) = reconcile(&f.app, &f.api_key, attempt_id).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "succeeded");
    assert!(
        body["psp_reference"].is_string(),
        "the original PSP reference should be recovered: {body}"
    );

    assert_eq!(attempt_status(&f.pool, attempt_id).await, "succeeded");
    assert_eq!(invoice_state(&f.pool, f.invoice_id).await, "paid");
    assert_eq!(
        attempt_count(&f.pool, f.invoice_id).await,
        1,
        "recovery must reuse the original attempt, never charge twice"
    );

    f.pool.close().await;
}

/// Two reconciliations racing on the same declined attempt must produce exactly one
/// `invoice.payment_failed` event. The failure path has no invoice-state update to
/// guard it, so the conditional status transition is what prevents the duplicate.
#[tokio::test]
async fn concurrent_reconciliation_emits_one_failure_event() {
    let f = fixture("Concurrent Reconcile Test").await;

    let provider_key = format!("dodo-concurrent-declined-{}", Uuid::new_v4());
    let seeded = seed_psp_payment(&provider_key, "tok_card_declined", 2500).await;
    assert_eq!(seeded["status"], "failed", "{seeded}");

    let attempt_id = insert_attempt(
        &f.pool,
        f.business_id,
        f.invoice_id,
        &provider_key,
        2500,
        "unknown",
    )
    .await;

    let mut handles = Vec::new();

    for _ in 0..4 {
        let app = f.app.clone();
        let api_key = f.api_key.clone();

        handles.push(tokio::spawn(async move {
            reconcile(&app, &api_key, attempt_id).await
        }));
    }

    for handle in handles {
        let (status, body) = handle.await.expect("reconcile task panicked");
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "failed");
    }

    let events: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM webhook_events
        WHERE business_id = $1
          AND event_type = 'invoice.payment_failed'
          AND payload->'data'->>'payment_attempt_id' = $2
        "#,
    )
    .bind(f.business_id)
    .bind(attempt_id.to_string())
    .fetch_one(&f.pool)
    .await
    .expect("failed to count failure events");

    assert_eq!(
        events, 1,
        "four concurrent reconciliations must not emit duplicate failure events"
    );

    assert_eq!(attempt_status(&f.pool, attempt_id).await, "failed");
    assert_eq!(invoice_state(&f.pool, f.invoice_id).await, "open");
    assert_eq!(attempt_count(&f.pool, f.invoice_id).await, 1);

    f.pool.close().await;
}
