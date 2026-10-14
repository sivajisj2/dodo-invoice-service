use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use sha2::{Digest, Sha256};
use sqlx;
use uuid::Uuid;

use crate::{error::AppError, state::AppState};

pub fn generate_api_key() -> String {
    let bytes: [u8; 32] = rand::random();

    format!("dodo_live_{}", hex::encode(bytes))
}

pub fn hash_api_key(api_key: &str) -> String {
    let digest = Sha256::digest(api_key.as_bytes());
    hex::encode(digest)
}
pub async fn require_api_key(
    State(state): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, AppError> {
    let api_key = request
        .headers()
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .ok_or(AppError::Unauthorized)?;

    let key_hash = hash_api_key(api_key);

    let business_id: Option<Uuid> = sqlx::query_scalar(
        r#"
        SELECT business_id
        FROM api_keys
        WHERE key_hash = $1
          AND revoked_at IS NULL
        "#,
    )
    .bind(key_hash)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| AppError::InternalServerError)?;

    let business_id = business_id.ok_or(AppError::Unauthorized)?;

    request.extensions_mut().insert(business_id);

    Ok(next.run(request).await)
}
