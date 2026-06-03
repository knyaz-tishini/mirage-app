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
use crate::pid_file;
use crate::supervisor::{
    ChildHandle, ChildState, ChildStatus, kill_tun, resolve_tun_binary,
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW,
};
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
    /// Cli binary (resolved at startup — could be cli1 or cli2 supervisor).
    pub cli_binary: PathBuf,
    /// Cli args with `{client_config_path}` already substituted.
    pub cli_args: Vec<String>,
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

// ── Tun spawn (platform-aware) ────────────────────────────────────────────────

/// Spawn the tun child process.
///
/// * Windows / Linux — direct [`spawn_child_with_flags`], same as before.
/// * macOS — wraps the invocation in an `osascript` admin-privilege prompt so
///   that the child can open a `utun` device without being root.
///   **Caveat**: the returned [`ChildHandle`] wraps the `osascript` process,
///   not `mirage2tun` itself.  The real tun process runs as root and is
///   backgrounded (`&` in the shell command); its PID is not tracked by
///   sentinel in v1.  Requires a GUI (Aqua) session — fails over SSH/headless.
fn spawn_tun(binary: &std::path::Path, tun_args: &[String], flags: u32) -> anyhow::Result<ChildHandle> {
    #[cfg(target_os = "macos")]
    {
        spawn_tun_macos(binary, tun_args)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mirror = crate::supervisor::unified_log_path();
        crate::supervisor::spawn_child_with_flags_and_mirror(
            &binary.to_path_buf(),
            tun_args,
            flags,
            mirror,
        )
    }
}

/// Quote a single token for Bourne shell using single-quote escaping.
/// `it's` → `'it'"'"'s'`
#[cfg(target_os = "macos")]
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\"'\"'");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(target_os = "macos")]
fn spawn_tun_macos(binary: &std::path::Path, tun_args: &[String]) -> anyhow::Result<ChildHandle> {
    use anyhow::Context;
    use std::process::Stdio;
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use crate::supervisor::RingBuffer;

    // Build a fully-quoted shell command:
    //   '/path/to/mirage2tun' '--arg1' 'val1' ... >/tmp/mirage2tun.log 2>&1 &
    let mut cmd_parts = Vec::with_capacity(tun_args.len() + 1);
    cmd_parts.push(sh_quote(&binary.to_string_lossy()));
    for arg in tun_args {
        cmd_parts.push(sh_quote(arg));
    }
    let shell_cmd = format!(
        "{} >/tmp/mirage2tun.log 2>&1 &",
        cmd_parts.join(" ")
    );

    // Escape double-quotes inside the shell command for the AppleScript string.
    let escaped = shell_cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("do shell script \"{}\" with administrator privileges", escaped);

    tracing::info!("[sentinel] tun macOS elevate: osascript prompt");

    let child = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "failed to spawn osascript for tun elevation")?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    Ok(ChildHandle {
        process: child,
        stdout_buf: Arc::new(Mutex::new(RingBuffer::new(64 * 1024))),
        stderr_buf: Arc::new(Mutex::new(RingBuffer::new(64 * 1024))),
        started_at: std::time::Instant::now(),
        started_at_ms: now_ms,
        adopted: false,
        pid_file: None,
    })
}

async fn connect(State(state): State<ServerState>) -> impl IntoResponse {
    let mut children = state.children.lock().await;
    if children.tun.is_some() {
        return (StatusCode::CONFLICT, "tun already running").into_response();
    }

    let flags = CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB;

    // Step 1: ensure cli is running.  Cli is required before tun activates
    // because tun forwards traffic to cli's SOCKS5 listeners.
    if children.cli.is_none() {
        info!("[sentinel] /connect: spawning cli first ({:?})", state.spawn_cfg.cli_binary);
        match crate::supervisor::spawn_child_with_flags_and_mirror(
            &state.spawn_cfg.cli_binary,
            &state.spawn_cfg.cli_args,
            flags,
            crate::supervisor::unified_log_path(),
        ) {
            Ok(handle) => {
                let cli_pid = handle.process.id();
                info!("[sentinel] cli spawned pid={:?}", cli_pid);
                if let Some(pid) = cli_pid {
                    if let Some(path) = pid_file::cli_pid_path() {
                        pid_file::write(&path, pid);
                    }
                }
                children.cli = Some(handle);
            }
            Err(e) => {
                warn!("[sentinel] cli spawn failed: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("cli spawn: {e}"))
                    .into_response();
            }
        }
        // Brief warmup so cli's SOCKS5 listeners are bound before tun
        // tries to forward.
        drop(children);
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        children = state.children.lock().await;
    }

    // Step 2: spawn tun.
    let binary = resolve_tun_binary(state.spawn_cfg.tun_binary.as_ref());
    let Some(binary) = binary else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "tun binary not found").into_response();
    };

    match spawn_tun(&binary, &state.spawn_cfg.tun_args, flags) {
        Ok(handle) => {
            let tun_pid = handle.process.id();
            info!("[sentinel] tun spawned pid={:?}", tun_pid);
            if let Some(pid) = tun_pid {
                if let Some(path) = pid_file::tun_pid_path() {
                    pid_file::write(&path, pid);
                }
            }
            children.tun = Some(handle);
            (StatusCode::OK, "cli + tun started").into_response()
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
        if !handle.adopted {
            kill_tun(&mut handle).await;
        }
        if let Some(path) = pid_file::tun_pid_path() { pid_file::remove(&path); }
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

/// POST /restart-self — trigger a soft self-restart of sentinel.
///
/// Children (cli, tun) have been spawned with `CREATE_BREAKAWAY_FROM_JOB` and
/// will survive sentinel's exit.  Their PID files remain on disk so the new
/// sentinel instance can adopt them via adopt-on-startup.
///
/// Flow:
/// 1. This handler returns 202 Accepted.
/// 2. Spawns a background task that sleeps 200 ms (gives HTTP response time to
///    flush) then calls `process::exit(0)`.
/// 3. The launcher's watchdog (d-watchdog / OS-level supervisor) sees the exit
///    and respawns a fresh sentinel binary.
/// 4. The fresh sentinel reads `cli.pid` / `tun.pid` and adopts the children.
async fn restart_self_handler(State(_state): State<ServerState>) -> impl IntoResponse {
    info!("[sentinel] /restart-self triggered — exiting for respawn");
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::process::exit(0);
    });
    (StatusCode::ACCEPTED, "restarting").into_response()
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
        .route("/restart-self", post(restart_self_handler))
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
