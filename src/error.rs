use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("internal server error")]
    InternalServerError,

    #[error("invalid request")]
    InvalidRequest,

    #[error("unauthorized")]
    Unauthorized,

    #[error("not found")]
    NotFound,

    #[error("conflict: {0}")]
    Conflict(String),
}

/// Turns a machine code such as `invoice_not_found` into `Invoice not found`,
/// so every endpoint gets a readable message without maintaining two lists.
fn humanize(code: &str) -> String {
    let spaced = code.replace('_', " ");
    let mut chars = spaced.chars();

    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

/// The single error envelope used by every endpoint:
/// `{"error": {"code": "...", "message": "..."}}`
pub fn error_body(code: &str) -> Value {
    json!({"error": {"code": code, "message": humanize(code)}})
}

/// Same envelope, for the cases where the generic message is not specific enough.
pub fn error_body_with(code: &str, message: &str) -> Value {
    json!({"error": {"code": code, "message": message}})
}

impl AppError {
    fn parts(&self) -> (StatusCode, &str) {
        match self {
            AppError::InternalServerError => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
            AppError::InvalidRequest => (StatusCode::BAD_REQUEST, "invalid_request"),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            AppError::Conflict(code) => (StatusCode::CONFLICT, code.as_str()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = self.parts();

        (status, Json(error_body(code))).into_response()
    }
}
