use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::sync::Arc;

/// Axum middleware that checks for an `Authorization: Bearer <psk>` header.
///
/// Requests that pass this middleware have a valid PSK.  All other requests
/// receive a `401 Unauthorized` JSON response.
pub async fn require_bearer(
    axum::extract::State(psk): axum::extract::State<Arc<String>>,
    req: Request,
    next: Next,
) -> Response {
    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let expected = format!("Bearer {}", psk.as_str());

    match auth_header {
        Some(h) if h == expected => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response(),
    }
}
