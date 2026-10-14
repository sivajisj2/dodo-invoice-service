use crate::customer::{create_customer, get_customer, list_customers};
use crate::{
    auth::require_api_key,
    business::{CreateBusinessRequest, create_business as create_business_record},
    error::AppError,
    state::AppState,
};
use axum::{
    Json, Router,
    extract::{Extension, State},
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use serde::Serialize;
use uuid::Uuid;

use crate::invoice_api::{create_invoice, get_invoice, list_invoices};

#[derive(Serialize)]
struct MeResponse {
    business_id: Uuid,
}

async fn create_business_handler(
    State(state): State<AppState>,
    Json(payload): Json<CreateBusinessRequest>,
) -> Result<(StatusCode, Json<crate::business::CreateBusinessResponse>), AppError> {
    if payload.name.trim().is_empty() {
        return Err(AppError::InvalidRequest);
    }

    let business = create_business_record(&state.db, payload)
        .await
        .map_err(|_| AppError::InternalServerError)?;

    Ok((StatusCode::CREATED, Json(business)))
}

async fn me_handler(Extension(business_id): Extension<Uuid>) -> Json<MeResponse> {
    Json(MeResponse { business_id })
}

async fn health_handler() -> StatusCode {
    StatusCode::OK
}

pub fn router(state: AppState) -> Router {
    let protected_routes = Router::new()
        .route("/v1/me", get(me_handler))
        .route("/v1/invoices/{id}/pay", post(crate::payment::pay_invoice))
        .route(
            "/v1/payment-attempts/{attempt_id}/reconcile",
            post(crate::payment::reconcile_payment),
        )
        .route("/v1/invoices", post(create_invoice).get(list_invoices))
        .route("/v1/invoices/{id}", get(get_invoice))
        .route("/v1/customers", post(create_customer).get(list_customers))
        .route("/v1/customers/{id}", get(get_customer))
        .route(
            "/v1/webhook-endpoints",
            post(crate::webhooks::create_endpoint),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ));

    Router::new()
        .route("/health", get(health_handler))
        .route("/docs", get(crate::openapi::docs))
        .route("/openapi.json", get(crate::openapi::spec))
        .route("/v1/businesses", post(create_business_handler))
        .merge(protected_routes)
        .with_state(state)
}
