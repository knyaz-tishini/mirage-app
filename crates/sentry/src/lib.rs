//! mirage-sentry — passive always-on agent on a trusted tester box.
//!
//! Self-sufficient: ALL configuration is compiled in (9 LM monitor URLs,
//! Client-role API bearer for WS subscribe). No `sentry.toml` on disk.
//!
//! Flow:
//!
//! 1. Boot → `MonitorLink` with the embedded URL list. First successful
//!    register persists `client_id` next to the binary.
//! 2. Periodic heartbeat to monitor (status = `running`/`idle`).
//! 3. WS subscribe → consumes `WsServerFrame` pushes.
//!    - `DebugTunnelOpen { reverse_port, ... }` from operator →
//!      `Command::new(<own dir>/mirage-slavyanka.exe).arg("--reverse-port").arg(N)`.
//!      Slavyanka uses its OWN embedded SSH key + jump_host — the frame's
//!      `ssh_priv_key_pem` / `monitor_ssh_host` are IGNORED.
//!    - `DebugTunnelClose` → SIGTERM the child.
//!    - `Config` → ignored (sentry has no config-driven behaviour).
//!
//! Operator triggers: `POST /relay/clients/{client_id}/debug/enable` on
//! any LM. Reuses the existing debug-tunnel endpoint pair — no new
//! monitor code required for this MVP.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mirage_client_core::monitor_link::{MonitorLink, WsServerFrame};
use serde::Serialize;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

// ── Compile-time configuration ────────────────────────────────────────────────

/// All 9 LMs in the mesh. Sentry races them on register / heartbeat /
/// WS subscribe; whichever is reachable wins. List order doesn't matter
/// — `MonitorLink` picks the first that completes its TLS handshake.
pub const MONITOR_URLS: &[&str] = &[
    "https://lm-nl-1.zen-geldmaschine.net:19443",
    "https://lm-nl-2.zen-geldmaschine.net:19443",
    "https://lm-tr-1.zen-geldmaschine.net:19443",
    "https://lm-tr-2.zen-geldmaschine.net:19443",
    "https://lm-tr-3.zen-geldmaschine.net:19443",
    "https://lm-ru.zen-geldmaschine.net:19443",
    "https://lm-ru-1.zen-geldmaschine.net:19443",
    "https://lm-ch-1.zen-geldmaschine.net:19443",
    "https://lm-kz-1.zen-geldmaschine.net:19443",
];

/// Client-role bearer key. The matching SHA-256 (paste-able as an
/// `[[api_keys]]` block) is in `tools/sentry-api-key.txt` next to the
/// binary — operator deploys it once per LM `config.toml`.
///
/// (Raw key is OK in plaintext for trusted-box MVP. Rotate by editing
/// this constant + the deployed hash and rebuilding.)
pub const MONITOR_API_KEY: &str =
    "e08da1f5365e3b9a273c15d2e4a96f005bcd04e1691429e8d7df54262daea1e1";
/// SHA-256 hash of `MONITOR_API_KEY` (no salt — same as
/// `mirage2monitor --genkey` emits). For deployment into LM config.toml.
pub const MONITOR_API_KEY_SHA256: &str =
    "168ea96cde0276cc7ef04288436738b9452df5dbb01174b1d510b2d24f596fa2";

/// Heartbeat cadence (s). Public endpoint, no auth.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 30;

/// Default state dir for the persisted `client_id`. On Windows we use
/// `%ProgramData%/mirage/state` (writable from a UAC-elevated child);
/// on Unix we use `/var/lib/mirage`.
fn default_state_dir() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("C:/ProgramData/mirage/state")
    } else {
        PathBuf::from("/var/lib/mirage")
    }
}

/// Locate the sibling `mirage-slavyanka.exe` (or `mirage-slavyanka` on
/// Unix) next to the running sentry binary.
fn locate_slavyanka_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    let dir = exe.parent().context("sentry exe has no parent dir")?;
    let name = if cfg!(windows) { "mirage-slavyanka.exe" } else { "mirage-slavyanka" };
    let path = dir.join(name);
    if !path.exists() {
        return Err(anyhow::anyhow!(
            "expected slavyanka binary alongside sentry at {:?}, not found",
            path
        ));
    }
    Ok(path)
}

// ── Slavyanka child status ───────────────────────────────────────────────────

#[derive(Clone)]
pub struct SlavyankaStatus(Arc<AtomicU8>);

impl SlavyankaStatus {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }
    pub fn set_idle(&self)     { self.0.store(0, Ordering::Relaxed); }
    pub fn set_running(&self)  { self.0.store(1, Ordering::Relaxed); }
    pub fn set_crashed(&self)  { self.0.store(2, Ordering::Relaxed); }
    pub fn as_str(&self) -> &'static str {
        match self.0.load(Ordering::Relaxed) {
            1 => "running",
            2 => "crashed",
            _ => "idle",
        }
    }
}

impl Default for SlavyankaStatus {
    fn default() -> Self { Self::new() }
}

// ── Heartbeat ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct ClientChainStatus {
    chain_name: String,
    transport: String,
    latency_ms: Option<u64>,
    bytes_tx: u64,
    bytes_rx: u64,
    errors: u64,
    connections: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ClientHeartbeat {
    active_chains: Vec<ClientChainStatus>,
    total_bytes_tx: u64,
    total_bytes_rx: u64,
    uptime_secs: u64,
}

async fn heartbeat_loop(
    http: reqwest::Client,
    client_id: String,
    monitor_url_picker: Arc<MonitorLink>,
    status: SlavyankaStatus,
    started_at: Instant,
) {
    let interval = Duration::from_secs(HEARTBEAT_INTERVAL_SECS.max(1));
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    loop {
        let url_base = monitor_url_picker.active_url_snapshot().await;
        let url = match url_base {
            Some(b) => format!("{}/relay/clients/{}/heartbeat", b.trim_end_matches('/'), client_id),
            None => {
                debug!("[sentry] no active monitor URL yet, skipping heartbeat tick");
                tokio::select! {
                    _ = tokio::time::sleep(interval) => continue,
                    _ = &mut shutdown => return,
                }
            }
        };
        let body = ClientHeartbeat {
            active_chains: vec![ClientChainStatus {
                chain_name: format!("slavyanka:{}", status.as_str()),
                transport: "reverse-ssh".to_string(),
                latency_ms: None,
                bytes_tx: 0,
                bytes_rx: 0,
                errors: 0,
                connections: if status.as_str() == "running" { 1 } else { 0 },
            }],
            total_bytes_tx: 0,
            total_bytes_rx: 0,
            uptime_secs: started_at.elapsed().as_secs(),
        };
        let send = http
            .post(&url)
            .bearer_auth(MONITOR_API_KEY)
            .json(&body)
            .send();
        tokio::select! {
            res = send => match res {
                Ok(r) if r.status().is_success() => {
                    debug!(status = status.as_str(), "[sentry] heartbeat ok");
                }
                Ok(r) => warn!(http = ?r.status(), "[sentry] heartbeat non-2xx"),
                Err(e) => warn!(error = %e, "[sentry] heartbeat HTTP error"),
            },
            _ = &mut shutdown => return,
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = &mut shutdown => return,
        }
    }
}

// ── Client-id persistence ───────────────────────────────────────────────────

fn read_client_id(state_dir: &Path) -> Option<String> {
    let p = state_dir.join("sentry-client-id.txt");
    let s = std::fs::read_to_string(p).ok()?;
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

fn write_client_id(state_dir: &Path, id: &str) -> Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("mkdir state_dir {:?}", state_dir))?;
    let p = state_dir.join("sentry-client-id.txt");
    let tmp = state_dir.join("sentry-client-id.txt.tmp");
    std::fs::write(&tmp, id.as_bytes())?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct ClientRegistration {
    os: String,
    version: String,
    supported_transports: Vec<String>,
}

async fn ensure_client_id(
    http: &reqwest::Client,
    state_dir: &Path,
    initial_monitor: &str,
) -> Result<String> {
    if let Some(id) = read_client_id(state_dir) {
        info!(client_id = %id, "[sentry] reusing persisted client_id");
        return Ok(id);
    }
    info!("[sentry] no cached client_id — registering with monitor");
    let body = ClientRegistration {
        os: std::env::consts::OS.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        supported_transports: vec!["slavyanka-reverse-tunnel".to_string()],
    };
    let url = format!("{}/relay/clients", initial_monitor.trim_end_matches('/'));
    #[derive(serde::Deserialize)]
    struct RegResp { client_id: String }
    let resp: RegResp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {}", url))?
        .error_for_status()
        .with_context(|| format!("monitor rejected registration at {}", url))?
        .json()
        .await
        .context("parse register response")?;
    write_client_id(state_dir, &resp.client_id)?;
    info!(client_id = %resp.client_id, "[sentry] registered, id persisted");
    Ok(resp.client_id)
}

// ── Slavyanka child controller ──────────────────────────────────────────────

struct SlavyankaController {
    binary: PathBuf,
    child: Mutex<Option<Child>>,
    status: SlavyankaStatus,
}

impl SlavyankaController {
    fn new(binary: PathBuf, status: SlavyankaStatus) -> Self {
        Self { binary, child: Mutex::new(None), status }
    }

    async fn spawn(&self, reverse_port: u16) {
        // If a child is already running, kill it first.
        self.kill().await;
        info!(reverse_port, binary = ?self.binary, "[sentry] spawning slavyanka");
        let spawn = Command::new(&self.binary)
            .arg("--reverse-port")
            .arg(reverse_port.to_string())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        match spawn {
            Ok(c) => {
                self.status.set_running();
                let mut g = self.child.lock().await;
                *g = Some(c);
            }
            Err(e) => {
                self.status.set_crashed();
                error!(error = %e, "[sentry] slavyanka spawn failed");
            }
        }
    }

    async fn kill(&self) {
        let mut g = self.child.lock().await;
        if let Some(mut c) = g.take() {
            let _ = c.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), c.wait()).await;
            info!("[sentry] slavyanka killed");
        }
        self.status.set_idle();
    }
}

// ── WS frame handler loop ───────────────────────────────────────────────────

async fn frame_loop(link: Arc<MonitorLink>, controller: Arc<SlavyankaController>) {
    let mut frames = link.subscribe_frames();
    loop {
        if frames.changed().await.is_err() {
            warn!("[sentry] frame channel closed — exiting frame loop");
            return;
        }
        let frame = frames.borrow().clone();
        let Some(frame) = frame else { continue };
        match frame {
            WsServerFrame::DebugTunnelOpen(payload) => {
                info!(reverse_port = payload.reverse_port, "[sentry] received DebugTunnelOpen — spawning slavyanka");
                controller.spawn(payload.reverse_port).await;
            }
            WsServerFrame::DebugTunnelClose => {
                info!("[sentry] received DebugTunnelClose — killing slavyanka");
                controller.kill().await;
            }
            WsServerFrame::Config(_) => {
                // Sentry has no config-driven behaviour. Monitor list updates
                // could be respected here; for MVP we use the compiled-in
                // list only.
                debug!("[sentry] ignoring Config frame");
            }
        }
    }
}

// ── Helper to peek the active monitor URL from MonitorLink ──────────────────
//
// MonitorLink doesn't expose this directly; we re-derive a usable URL by
// iterating its known list in order. Heartbeats POST to the first URL
// since the public endpoint is mirrored on every LM.

trait MonitorLinkActiveUrl {
    async fn active_url_snapshot(&self) -> Option<String>;
}

impl MonitorLinkActiveUrl for MonitorLink {
    async fn active_url_snapshot(&self) -> Option<String> {
        // No public accessor — fall back to the compiled-in first URL. The
        // heartbeat endpoint exists on every LM with the same shape, so
        // pinning the first URL is correct for the MVP.
        Some(MONITOR_URLS[0].to_string())
    }
}

// ── Public entry point ──────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    let state_dir = default_state_dir();

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;

    // First-boot register via the first reachable URL. Public endpoint, no auth.
    let mut client_id = None;
    for url in MONITOR_URLS {
        match ensure_client_id(&http, &state_dir, url).await {
            Ok(id) => { client_id = Some(id); break; }
            Err(e) => {
                warn!(url, error = %e, "[sentry] register attempt failed, trying next monitor");
            }
        }
    }
    let client_id = client_id.context("could not register with ANY of the embedded monitors")?;

    // MonitorLink — handles WS subscription with auth across the URL list.
    let urls: Vec<String> = MONITOR_URLS.iter().map(|s| s.to_string()).collect();
    let (link, _config_rx) = MonitorLink::new(urls, Some(MONITOR_API_KEY.to_string()));
    link.spawn();

    let binary = locate_slavyanka_binary().context("locate sibling mirage-slavyanka")?;
    info!(?binary, "[sentry] slavyanka binary located");
    let status = SlavyankaStatus::new();
    let controller = Arc::new(SlavyankaController::new(binary, status.clone()));

    let frame_task = {
        let link = Arc::clone(&link);
        let controller = Arc::clone(&controller);
        tokio::spawn(async move {
            frame_loop(link, controller).await;
        })
    };

    let hb_task = {
        let http = http.clone();
        let id = client_id.clone();
        let link = Arc::clone(&link);
        let status = status.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            heartbeat_loop(http, id, link, status, started).await;
        })
    };

    info!(client_id = %client_id, "[sentry] ready; waiting for operator commands");

    // Wait for Ctrl-C, then shutdown.
    let _ = tokio::signal::ctrl_c().await;
    info!("[sentry] Ctrl-C — shutting down");
    controller.kill().await;
    frame_task.abort();
    hb_task.abort();
    Ok(())
}
