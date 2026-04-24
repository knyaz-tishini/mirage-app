use std::sync::Arc;

use axum::{middleware, routing::{get, post, put}, Router};
use tracing::info;

use crate::management::{
    auth::require_bearer,
    handlers::{
        get_chain_stats, get_chains, get_health, get_logs, get_status, post_chain_switch,
        put_camouflage, put_cover_traffic, put_mode, put_shaping,
    },
};
use crate::runtime::ClientRuntime;

/// Start the embedded management HTTP server.
///
/// Binds to `addr` and serves:
/// - `GET  /admin/health`              — public, no auth required.
/// - `GET  /admin/status`              — requires `Authorization: Bearer <psk>`.
/// - `GET  /admin/logs`                — requires `Authorization: Bearer <psk>`.
/// - `GET  /admin/chains`              — list all chains with pool stats.
/// - `GET  /admin/chain/{name}/stats`  — per-chain pool stats.
/// - `POST /admin/chain/switch`        — requires `Authorization: Bearer <psk>`.
/// - `PUT  /admin/mode`                — requires `Authorization: Bearer <psk>`.
/// - `PUT  /admin/camouflage`          — requires `Authorization: Bearer <psk>`.
/// - `PUT  /admin/shaping`             — requires `Authorization: Bearer <psk>`.
/// - `PUT  /admin/cover-traffic`       — requires `Authorization: Bearer <psk>`.
///
/// # Errors
///
/// Returns an error if the socket cannot be bound.
pub async fn run(
    runtime: Arc<ClientRuntime>,
    psk: Arc<String>,
    addr: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Public routes — no authentication.
    let public_routes = Router::new().route("/admin/health", get(get_health));

    // Authenticated routes.
    let authed_routes = Router::new()
        .route("/admin/status", get(get_status))
        .route("/admin/logs", get(get_logs))
        .route("/admin/chains", get(get_chains))
        .route("/admin/chain/{name}/stats", get(get_chain_stats))
        .route("/admin/chain/switch", post(post_chain_switch))
        .route("/admin/mode", put(put_mode))
        .route("/admin/camouflage", put(put_camouflage))
        .route("/admin/shaping", put(put_shaping))
        .route("/admin/cover-traffic", put(put_cover_traffic))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&psk),
            require_bearer,
        ))
        .with_state(Arc::clone(&runtime));

    let app = public_routes.merge(authed_routes);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("management server: failed to bind {addr}: {e}"))?;

    info!(addr = %addr, "client management server listening");

    axum::serve(listener, app)
        .await
        .map_err(|e| format!("management server error: {e}").into())
}
