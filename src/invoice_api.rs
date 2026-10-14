use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;
use uuid::Uuid;

use crate::{error::AppError, state::AppState};

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    pub customer_id: Uuid,
    pub line_items: Vec<CreateInvoiceLineItem>,
    pub due_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceLineItem {
    pub description: String,
    pub quantity: i32,
    pub unit_amount_cents: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct Invoice {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub total_cents: i64,
    pub currency: String,
    pub state: String,
    pub due_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct InvoiceLineItem {
    pub id: Uuid,
    pub description: String,
    pub quantity: i32,
    pub unit_amount_cents: i64,
}

#[derive(Debug, Serialize)]
pub struct InvoiceDetails {
    pub invoice: Invoice,
    pub line_items: Vec<InvoiceLineItem>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct InvoiceSummary {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub total_cents: i64,
    pub currency: String,
    pub state: String,
    pub due_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct ListInvoicesQuery {
    pub state: Option<String>,
}

fn calculate_total(items: &[CreateInvoiceLineItem]) -> Result<i64, AppError> {
    if items.is_empty() {
        return Err(AppError::InvalidRequest);
    }

    let mut total = 0_i64;

    for item in items {
        if item.description.trim().is_empty() || item.quantity <= 0 || item.unit_amount_cents <= 0 {
            return Err(AppError::InvalidRequest);
        }

        let line_total = item
            .unit_amount_cents
            .checked_mul(i64::from(item.quantity))
            .ok_or(AppError::InvalidRequest)?;

        total = total
            .checked_add(line_total)
            .ok_or(AppError::InvalidRequest)?;
    }

    Ok(total)
}

pub async fn create_invoice(
    State(state): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Json(payload): Json<CreateInvoiceRequest>,
) -> Result<(StatusCode, Json<InvoiceDetails>), AppError> {
    let total_cents = calculate_total(&payload.line_items)?;

    let mut tx = state.db.begin().await.map_err(|error| {
        tracing::error!(?error, "failed to begin invoice transaction");
        AppError::InternalServerError
    })?;

    // Verify that this customer belongs to the authenticated business.
    let customer_exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM customers
            WHERE id = $1 AND business_id = $2
        )
        "#,
    )
    .bind(payload.customer_id)
    .bind(business_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| {
        tracing::error!(?error, "failed to verify customer ownership");
        AppError::InternalServerError
    })?;

    if !customer_exists {
        return Err(AppError::NotFound);
    }

    let invoice_id = Uuid::new_v4();

    let invoice = sqlx::query_as::<_, Invoice>(
        r#"
        INSERT INTO invoices (
            id, business_id, customer_id, total_cents,
            currency, state, due_at
        )
        VALUES ($1, $2, $3, $4, 'USD', 'open', $5)
        RETURNING
            id, customer_id, total_cents, currency, state,
            due_at, created_at, updated_at
        "#,
    )
    .bind(invoice_id)
    .bind(business_id)
    .bind(payload.customer_id)
    .bind(total_cents)
    .bind(payload.due_at)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| {
        tracing::error!(?error, "failed to insert invoice");
        AppError::InternalServerError
    })?;

    let mut saved_items = Vec::with_capacity(payload.line_items.len());

    for item in &payload.line_items {
        let saved_item = sqlx::query_as::<_, InvoiceLineItem>(
            r#"
            INSERT INTO invoice_line_items (
                id, invoice_id, description, quantity,
                unit_amount_cents
            )
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id, description, quantity, unit_amount_cents
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(invoice_id)
        .bind(item.description.trim())
        .bind(item.quantity)
        .bind(item.unit_amount_cents)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| {
            tracing::error!(?error, "failed to insert invoice line item");
            AppError::InternalServerError
        })?;

        saved_items.push(saved_item);
    }

    let event_id = Uuid::new_v4();

    let event_payload = json!({
        "id": event_id,
        "type": "invoice.created",
        "data": {
            "invoice_id": invoice_id,
            "customer_id": payload.customer_id,
            "total_cents": total_cents,
            "currency": invoice.currency,
            "state": invoice.state,
            "due_at": invoice.due_at
        }
    });

    sqlx::query(
        r#"
        INSERT INTO webhook_events (
            id, business_id, event_type, payload
        )
        VALUES ($1, $2, 'invoice.created', $3)
        "#,
    )
    .bind(event_id)
    .bind(business_id)
    .bind(&event_payload)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        tracing::error!(?error, "failed to create invoice.created event");
        AppError::InternalServerError
    })?;

    sqlx::query(
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
    .await
    .map_err(|error| {
        tracing::error!(?error, "failed to create invoice.created deliveries");
        AppError::InternalServerError
    })?;

    tx.commit().await.map_err(|error| {
        tracing::error!(?error, "failed to commit invoice transaction");
        AppError::InternalServerError
    })?;

    Ok((
        StatusCode::CREATED,
        Json(InvoiceDetails {
            invoice,
            line_items: saved_items,
        }),
    ))
}

pub async fn get_invoice(
    State(state): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceDetails>, AppError> {
    let invoice = sqlx::query_as::<_, Invoice>(
        r#"
        SELECT
            id, customer_id, total_cents, currency, state,
            due_at, created_at, updated_at
        FROM invoices
        WHERE id = $1 AND business_id = $2
        "#,
    )
    .bind(invoice_id)
    .bind(business_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(?error, "failed to retrieve invoice");
        AppError::InternalServerError
    })?
    .ok_or(AppError::NotFound)?;

    let line_items = sqlx::query_as::<_, InvoiceLineItem>(
        r#"
        SELECT id, description, quantity, unit_amount_cents
        FROM invoice_line_items
        WHERE invoice_id = $1
        ORDER BY id
        "#,
    )
    .bind(invoice_id)
    .fetch_all(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(?error, "failed to retrieve invoice line items");
        AppError::InternalServerError
    })?;

    Ok(Json(InvoiceDetails {
        invoice,
        line_items,
    }))
}

pub async fn list_invoices(
    State(state): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Query(query): Query<ListInvoicesQuery>,
) -> Result<Json<Vec<InvoiceSummary>>, AppError> {
    if let Some(ref requested_state) = query.state {
        let valid = matches!(
            requested_state.as_str(),
            "draft" | "open" | "paid" | "void" | "uncollectible"
        );

        if !valid {
            return Err(AppError::InvalidRequest);
        }
    }

    let invoices = sqlx::query_as::<_, InvoiceSummary>(
        r#"
        SELECT
            id, customer_id, total_cents, currency, state,
            due_at, created_at
        FROM invoices
        WHERE business_id = $1
          AND ($2::text IS NULL OR state = $2)
        ORDER BY created_at DESC, id DESC
        LIMIT 100
        "#,
    )
    .bind(business_id)
    .bind(query.state)
    .fetch_all(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(?error, "failed to list invoices");
        AppError::InternalServerError
    })?;

    Ok(Json(invoices))
}
