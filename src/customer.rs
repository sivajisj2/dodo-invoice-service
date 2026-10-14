use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{error::AppError, state::AppState};

#[derive(Debug, Deserialize)]
pub struct CreateCustomerRequest {
    pub name: String,
    pub email: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub email: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn create_customer(
    State(state): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Json(payload): Json<CreateCustomerRequest>,
) -> Result<(StatusCode, Json<Customer>), AppError> {
    let name = payload.name.trim();

    if name.is_empty() {
        return Err(AppError::InvalidRequest);
    }

    let email = payload
        .email
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());

    let customer_id = Uuid::new_v4();

    let customer = sqlx::query_as::<_, Customer>(
        r#"
        INSERT INTO customers (id, business_id, name, email)
        VALUES ($1, $2, $3, $4)
        RETURNING id, name, email, created_at
        "#,
    )
    .bind(customer_id)
    .bind(business_id)
    .bind(name)
    .bind(email)
    .fetch_one(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(?error, "failed to create customer");
        AppError::InternalServerError
    })?;

    Ok((StatusCode::CREATED, Json(customer)))
}

pub async fn get_customer(
    State(state): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Path(customer_id): Path<Uuid>,
) -> Result<Json<Customer>, AppError> {
    let customer = sqlx::query_as::<_, Customer>(
        r#"
        SELECT id, name, email, created_at
        FROM customers
        WHERE id = $1 AND business_id = $2
        "#,
    )
    .bind(customer_id)
    .bind(business_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(?error, "failed to retrieve customer");
        AppError::InternalServerError
    })?
    .ok_or(AppError::NotFound)?;

    Ok(Json(customer))
}

pub async fn list_customers(
    State(state): State<AppState>,
    Extension(business_id): Extension<Uuid>,
) -> Result<Json<Vec<Customer>>, AppError> {
    let customers = sqlx::query_as::<_, Customer>(
        r#"
        SELECT id, name, email, created_at
        FROM customers
        WHERE business_id = $1
        ORDER BY created_at DESC, id DESC
        LIMIT 100
        "#,
    )
    .bind(business_id)
    .fetch_all(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(?error, "failed to list customers");
        AppError::InternalServerError
    })?;

    Ok(Json(customers))
}
