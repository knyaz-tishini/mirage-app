//! HTTP status/control server for mirage-l0-sentinel.
//!
//! Binds on `127.0.0.1:19420`.
//!
//! Routes:
//!   GET  /health             → 200 "ok"
//!   GET  /status             → JSON phase + child states
//!   POST /connect            → spawn tun (409 if already running)
//!   POST /disconnect         → kill tun (idempotent)
//!   GET  /logs/cli?tail=N    → last N bytes from cli stderr (default 8KB, max 64KB)
//!   GET  /logs/tun?tail=N    → last N bytes from tun stderr
//!   POST /onboard            → parse mirage://onboard?... URL, patch sentinel.toml,
//!                              signal re-bootstrap

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};
use tracing::{info, warn};

use crate::onboarding::{OnboardRequest, default_toml_path, log_onboard_result, patch_sentinel_toml};
use crate::supervisor::{ChildState, ChildStatus, kill_tun, resolve_tun_binary, spawn_child};
use crate::SentinelPhase;

// ── Shared server state ───────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ServerState {
    pub phase_rx: watch::Receiver<SentinelPhase>,
    pub started_at_ms: u128,
    pub last_update: Arc<std::sync::Mutex<u128>>,
    /// Shared child supervisor state (cli + tun).
    pub children: Arc<Mutex<ChildState>>,
    /// Config needed to spawn tun on /connect.
    pub spawn_cfg: Arc<TunSpawnCfg>,
    /// For status: monitor URL + reachability snapshot.
    pub monitor_info: Arc<std::sync::Mutex<MonitorInfo>>,
    /// Signal re-bootstrap on successful /onboard.
    pub reboot_tx: Arc<watch::Sender<Option<RebootPayload>>>,
    /// Which cli variant is currently active (`"cli1"` or `"cli2"`).
    pub cli_variant: String,
}

#[derive(Clone)]
pub struct TunSpawnCfg {
    pub tun_binary: Option<PathBuf>,
    pub tun_args: Vec<String>,
}

#[derive(Clone, Serialize, Default)]
pub struct MonitorInfo {
    pub monitor_url: String,
    pub monitor_reachable: bool,
}

/// Payload sent over the reboot channel when /onboard succeeds.
#[derive(Clone, Debug)]
pub struct RebootPayload {
    pub monitor_url: String,
    pub api_key: String,
    pub client_id: String,
}

// ── Response shapes ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    #[serde(flatten)]
    phase: SentinelPhase,
    uptime_ms: u128,
    last_update_at_ms: u128,
    cli: ChildStatus,
    tun: ChildStatus,
    monitor_url: String,
    monitor_reachable: bool,
    /// Which cli variant is running: `"cli1"` (legacy) or `"cli2"` (split-arch).
    cli_variant: String,
}

// ── Query params ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TailQuery {
    tail: Option<usize>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    "ok"
}

async fn status_handler(State(state): State<ServerState>) -> impl IntoResponse {
    let now_ms = unix_now_ms();
    let phase = state.phase_rx.borrow().clone();
    let last_update_at_ms = *state.last_update.lock().unwrap_or_else(|e| e.into_inner());
    let children = state.children.lock().await;
    let info = state.monitor_info.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let body = StatusResponse {
        phase,
        uptime_ms: now_ms.saturating_sub(state.started_at_ms),
        last_update_at_ms,
        cli: children.cli_status(),
        tun: children.tun_status(),
        monitor_url: info.monitor_url,
        monitor_reachable: info.monitor_reachable,
        cli_variant: state.cli_variant.clone(),
    };
    Json(body)
}

async fn connect(State(state): State<ServerState>) -> impl IntoResponse {
    let mut children = state.children.lock().await;
    if children.tun.is_some() {
        return (StatusCode::CONFLICT, "tun already running").into_response();
    }

    let binary = resolve_tun_binary(state.spawn_cfg.tun_binary.as_ref());
    let Some(binary) = binary else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "tun binary not found").into_response();
    };

    match spawn_child(&binary, &state.spawn_cfg.tun_args) {
        Ok(handle) => {
            info!("[sentinel] tun spawned pid={:?}", handle.process.id());
            children.tun = Some(handle);
            (StatusCode::OK, "tun started").into_response()
        }
        Err(e) => {
            warn!("[sentinel] tun spawn failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

async fn disconnect(State(state): State<ServerState>) -> impl IntoResponse {
    let mut children = state.children.lock().await;
    if let Some(mut handle) = children.tun.take() {
        kill_tun(&mut handle).await;
        info!("[sentinel] tun disconnected");
    }
    (StatusCode::OK, "ok").into_response()
}

async fn logs_cli(
    State(state): State<ServerState>,
    Query(params): Query<TailQuery>,
) -> impl IntoResponse {
    let n = params.tail.unwrap_or(8 * 1024).min(64 * 1024);
    let children = state.children.lock().await;
    match &children.cli {
        Some(h) => {
            let tail = h.stderr_buf.lock().await.tail_utf8(n);
            (StatusCode::OK, tail).into_response()
        }
        None => (StatusCode::NOT_FOUND, "cli not running").into_response(),
    }
}

async fn logs_tun(
    State(state): State<ServerState>,
    Query(params): Query<TailQuery>,
) -> impl IntoResponse {
    let n = params.tail.unwrap_or(8 * 1024).min(64 * 1024);
    let children = state.children.lock().await;
    match &children.tun {
        Some(h) => {
            let tail = h.stderr_buf.lock().await.tail_utf8(n);
            (StatusCode::OK, tail).into_response()
        }
        None => (StatusCode::NOT_FOUND, "tun not running").into_response(),
    }
}

async fn onboard_handler(
    State(state): State<ServerState>,
    Json(body): Json<OnboardRequest>,
) -> impl IntoResponse {
    use crate::onboarding::parse_onboard_url;

    let payload = match parse_onboard_url(&body.url) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e })),
            )
                .into_response();
        }
    };

    // Patch sentinel.toml on disk
    if let Some(toml_path) = default_toml_path() {
        let result = patch_sentinel_toml(&toml_path, &payload);
        log_onboard_result(&result);
        if let Err(e) = result {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e })),
            )
                .into_response();
        }
    } else {
        warn!("[onboard] cannot determine sentinel.toml path");
    }

    // Signal re-bootstrap
    let reboot = RebootPayload {
        monitor_url: payload.monitor_url.clone(),
        api_key: payload.api_key.clone(),
        client_id: payload.client_id.clone(),
    };
    let _ = state.reboot_tx.send(Some(reboot));

    info!("[onboard] re-bootstrap triggered for client_id={}", payload.client_id);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "client_id": payload.client_id })),
    )
        .into_response()
}

// ── Phase watch ───────────────────────────────────────────────────────────────

/// Create a watch channel seeded with [`SentinelPhase::Starting`].
pub fn phase_channel() -> (watch::Sender<SentinelPhase>, watch::Receiver<SentinelPhase>) {
    watch::channel(SentinelPhase::Starting)
}

/// Create a reboot channel (starts as `None`).
pub fn reboot_channel() -> (
    Arc<watch::Sender<Option<RebootPayload>>>,
    watch::Receiver<Option<RebootPayload>>,
) {
    let (tx, rx) = watch::channel(None);
    (Arc::new(tx), rx)
}

// ── Spawn ─────────────────────────────────────────────────────────────────────

/// Spawn the status server. Returns a [`PhasePublisher`] for phase updates.
pub fn spawn(
    phase_rx: watch::Receiver<SentinelPhase>,
    phase_tx: watch::Sender<SentinelPhase>,
    children: Arc<Mutex<ChildState>>,
    spawn_cfg: TunSpawnCfg,
    monitor_info: Arc<std::sync::Mutex<MonitorInfo>>,
    reboot_tx: Arc<watch::Sender<Option<RebootPayload>>>,
    cli_variant: String,
) -> PhasePublisher {
    let started_at_ms = unix_now_ms();
    let last_update = Arc::new(std::sync::Mutex::new(started_at_ms));
    let last_update_clone = Arc::clone(&last_update);

    let state = ServerState {
        phase_rx,
        started_at_ms,
        last_update,
        children,
        spawn_cfg: Arc::new(spawn_cfg),
        monitor_info,
        reboot_tx,
        cli_variant,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status_handler))
        .route("/connect", post(connect))
        .route("/disconnect", post(disconnect))
        .route("/logs/cli", get(logs_cli))
        .route("/logs/tun", get(logs_tun))
        .route("/onboard", post(onboard_handler))
        .with_state(state);

    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:19420").await {
            Ok(l) => l,
            Err(e) => {
                warn!("[sentinel-status] bind 127.0.0.1:19420 failed: {} — status server disabled", e);
                return;
            }
        };
        info!("[sentinel-status] listening on 127.0.0.1:19420");
        if let Err(e) = axum::serve(listener, app).await {
            warn!("[sentinel-status] server error: {}", e);
        }
    });

    PhasePublisher { tx: phase_tx, last_update: last_update_clone }
}

// ── PhasePublisher ────────────────────────────────────────────────────────────

pub struct PhasePublisher {
    tx: watch::Sender<SentinelPhase>,
    last_update: Arc<std::sync::Mutex<u128>>,
}

impl PhasePublisher {
    pub fn set(&self, phase: SentinelPhase) {
        let now = unix_now_ms();
        *self.last_update.lock().unwrap_or_else(|e| e.into_inner()) = now;
        let _ = self.tx.send(phase);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn unix_now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
