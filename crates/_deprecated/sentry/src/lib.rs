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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info, warn};

// ── Compile-time configuration ────────────────────────────────────────────────

/// All 9 LMs in the mesh. Sentry races them on register / heartbeat /
/// WS subscribe; whichever is reachable wins. List order doesn't matter
/// — `MonitorLink` picks the first that completes its TLS handshake.
/// All 9 LMs. DNS form only — `reqwest`'s resolver gets pre-populated
/// (host → IP) below so TLS cert validation works against the hostname
/// AND we bypass the tester's potentially-broken OS-level DNS.
///
/// Source: `mirage2monitor/CLAUDE.md` "9-LM full-clique mesh".
#[cfg(feature = "server-lm-mode")]
pub const SENTRY_ROLE: &str = "sentry-server-lm";
#[cfg(feature = "server-node-mode")]
pub const SENTRY_ROLE: &str = "sentry-server-node";
#[cfg(any(
    feature = "client-mode",
    not(any(feature = "server-lm-mode", feature = "server-node-mode"))
))]
pub const SENTRY_ROLE: &str = "sentry-client";

/// Optional human label baked at build time via SENTRY_SHORTNAME env var.
/// Examples: "tester1", "tester2", "lm-nl-1", "box-91.218".
pub const SENTRY_SHORTNAME: Option<&str> = option_env!("SENTRY_SHORTNAME");

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

/// `(hostname, ip:port)` pairs fed into `reqwest::ClientBuilder::resolve`.
/// IPs from `mirage2monitor/CLAUDE.md` "9-LM full-clique mesh" table.
const HOST_TO_IP: &[(&str, &str)] = &[
    ("lm-nl-1.zen-geldmaschine.net", "45.12.212.226:19443"),
    ("lm-nl-2.zen-geldmaschine.net", "151.243.173.104:19443"),
    ("lm-tr-1.zen-geldmaschine.net", "103.231.75.166:19443"),
    ("lm-tr-2.zen-geldmaschine.net", "82.26.94.247:19443"),
    ("lm-tr-3.zen-geldmaschine.net", "103.231.75.144:19443"),
    ("lm-ru.zen-geldmaschine.net",   "193.187.94.74:19443"),
    ("lm-ru-1.zen-geldmaschine.net", "91.218.245.247:19443"),
    ("lm-ch-1.zen-geldmaschine.net", "45.59.119.139:19443"),
    ("lm-kz-1.zen-geldmaschine.net", "91.212.166.95:19443"),
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

/// Default state dir for persisted state. Uses `%LOCALAPPDATA%/mirage/state`
/// on Windows (writable for the current user — no UAC), `~/.local/share/
/// mirage` on Unix, falls back to the system temp dir as a last resort.
fn default_state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            if !local.is_empty() {
                return PathBuf::from(local).join("mirage").join("state");
            }
        }
    }
    #[cfg(unix)]
    {
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home).join(".local/share/mirage");
            }
        }
    }
    std::env::temp_dir().join("mirage").join("state")
}

// ── Slavyanka task status ───────────────────────────────────────────────────

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

/// Heartbeat to ALL registered LMs in parallel every interval.
async fn heartbeat_loop(
    http: reqwest::Client,
    registrations: Vec<LmRegistration>,
    status: SlavyankaStatus,
    started_at: Instant,
) {
    let interval = Duration::from_secs(HEARTBEAT_INTERVAL_SECS.max(1));
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    loop {
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
        let body_arc = Arc::new(body);
        let mut tasks = Vec::new();
        for reg in &registrations {
            let url = format!(
                "{}/relay/clients/{}/heartbeat",
                reg.url.trim_end_matches('/'),
                reg.client_id
            );
            let http = http.clone();
            let body = Arc::clone(&body_arc);
            let lm_label = reg.url.clone();
            tasks.push(tokio::spawn(async move {
                match http.post(&url).bearer_auth(MONITOR_API_KEY).json(&*body).send().await {
                    Ok(r) if r.status().is_success() => debug!(lm = %lm_label, "[sentry] heartbeat ok"),
                    Ok(r) => warn!(lm = %lm_label, http = ?r.status(), "[sentry] heartbeat non-2xx"),
                    Err(e) => debug!(lm = %lm_label, error = %e, "[sentry] heartbeat HTTP error"),
                }
            }));
        }
        let heartbeats = futures::future::join_all(tasks);
        tokio::select! {
            _ = heartbeats => {}
            _ = &mut shutdown => return,
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = &mut shutdown => return,
        }
    }
}

// ── State persistence ───────────────────────────────────────────────────────

/// One LM registration. `client_id` is what THAT LM minted for us; it's
/// not usable against any other LM (peer replication is async and
/// unreliable). We persist the full list so reachability survives
/// individual LMs going dark.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct LmRegistration {
    url: String,
    client_id: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct SentryState {
    /// All LMs where we successfully registered, paired with each one's
    /// minted client_id. We hold WS subscription + heartbeat against
    /// every entry simultaneously so an operator hitting `/debug/enable`
    /// on ANY of these LMs reaches us.
    registrations: Vec<LmRegistration>,
}

fn state_file(state_dir: &Path) -> PathBuf {
    state_dir.join("sentry-state.json")
}

fn read_state(state_dir: &Path) -> Option<SentryState> {
    let p = state_file(state_dir);
    let s = std::fs::read_to_string(&p).ok()?;
    serde_json::from_str(&s).ok()
}

fn write_state(state_dir: &Path, state: &SentryState) -> Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("mkdir state_dir {:?}", state_dir))?;
    let p = state_file(state_dir);
    let tmp = state_dir.join("sentry-state.json.tmp");
    let bytes = serde_json::to_vec_pretty(state).context("serialize SentryState")?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct ClientRegistration {
    os: String,
    version: String,
    supported_transports: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sentry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shortname: Option<String>,
}

/// Read or generate a stable per-box sentry id, persisted next to the other
/// sentry state. The same id is sent to every LM as `sentry_id` so all 9
/// `LmRegistration` rows end up with one common `client_id` — operator can
/// recognise "this is the same physical sentry, registered everywhere".
fn ensure_sentry_id(state_dir: &Path) -> Result<String> {
    let p = state_dir.join("sentry-id.txt");
    if let Ok(s) = std::fs::read_to_string(&p) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("mkdir state_dir {:?}", state_dir))?;
    let id = uuid::Uuid::new_v4().to_string();
    let tmp = state_dir.join("sentry-id.txt.tmp");
    std::fs::write(&tmp, &id).with_context(|| format!("write tmp {:?}", tmp))?;
    std::fs::rename(&tmp, &p).with_context(|| format!("rename {:?} -> {:?}", tmp, p))?;
    Ok(id)
}

#[derive(serde::Deserialize)]
struct RegResp {
    client_id: String,
}

/// Register on ALL 9 LMs in parallel, collect every success. Sentry needs
/// to be reachable from any LM the operator might choose, so we hold
/// concurrent WS + heartbeat against each one (their per-LM client_ids
/// can differ since LM peer replication is unreliable; we track every
/// pair). Errors on failed LMs are non-fatal — partial coverage still
/// works for ops on the LMs that DID accept us.
async fn ensure_state(
    http: &reqwest::Client,
    state_dir: &Path,
    sentry_id: &str,
) -> Result<SentryState> {
    if let Some(st) = read_state(state_dir) {
        // Invalidate stale state from pre-sentry_id builds: if any cached row
        // disagrees with the persisted `sentry-id.txt`, force fresh register
        // so all 9 LMs converge on the stable id.
        let all_match = !st.registrations.is_empty()
            && st.registrations.iter().all(|r| r.client_id == sentry_id);
        if all_match {
            info!(
                count = st.registrations.len(),
                "[sentry] reusing persisted state ({} LMs, stable id confirmed)",
                st.registrations.len()
            );
            return Ok(st);
        }
        warn!(
            cached = st.registrations.len(),
            "[sentry] cached state predates stable sentry_id — re-registering everywhere"
        );
    }
    info!("[sentry] no usable cached state — registering on ALL 9 monitors in parallel");

    let body = Arc::new(ClientRegistration {
        os: std::env::consts::OS.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        supported_transports: vec!["slavyanka-reverse-tunnel".to_string()],
        sentry_id: Some(sentry_id.to_string()),
        role: Some(SENTRY_ROLE.to_string()),
        shortname: SENTRY_SHORTNAME.map(String::from),
    });

    let mut tasks = Vec::new();
    for url in MONITOR_URLS {
        let http = http.clone();
        let body = Arc::clone(&body);
        let url_owned = (*url).to_string();
        tasks.push(tokio::spawn(async move {
            let endpoint = format!("{}/relay/clients", url_owned.trim_end_matches('/'));
            let resp = http
                .post(&endpoint)
                .json(&*body)
                .send()
                .await
                .map_err(|e| (url_owned.clone(), anyhow::anyhow!("{e}")))?;
            if !resp.status().is_success() {
                return Err((url_owned, anyhow::anyhow!("HTTP {}", resp.status())));
            }
            let parsed: RegResp = resp
                .json()
                .await
                .map_err(|e| (url_owned.clone(), anyhow::anyhow!("parse: {e}")))?;
            Ok::<LmRegistration, (String, anyhow::Error)>(LmRegistration {
                url: url_owned,
                client_id: parsed.client_id,
            })
        }));
    }

    // Collect ALL — wait for every task, keep successes.
    let mut registrations = Vec::new();
    for h in tasks {
        match h.await {
            Ok(Ok(reg)) => {
                info!(url = %reg.url, client_id = %reg.client_id, "[sentry] registered");
                registrations.push(reg);
            }
            Ok(Err((url, e))) => {
                warn!(url, error = %e, "[sentry] register failed on this LM");
            }
            Err(je) => warn!(error = %je, "[sentry] register task panicked"),
        }
    }

    if registrations.is_empty() {
        return Err(anyhow::anyhow!("ALL 9 monitors rejected registration"));
    }

    let st = SentryState { registrations };
    if let Err(e) = write_state(state_dir, &st) {
        warn!(error = %e, "[sentry] failed to persist state — continuing in-memory");
    }
    info!(count = st.registrations.len(), "[sentry] persisted state");
    Ok(st)
}

// ── Slavyanka task controller ─────────────────────────────────────────────
//
// Slavyanka lives as an in-process tokio task — NOT a child binary. The
// `mirage-slavyanka` library exposes `pub async fn run(reverse_port)`;
// we spawn it under a `JoinHandle` and abort the handle on the close
// command. This eliminates the path-lookup, child-process, separate-log
// failure modes the operator pushed back on.

struct SlavyankaController {
    handle: Mutex<Option<JoinHandle<()>>>,
    status: SlavyankaStatus,
}

impl SlavyankaController {
    fn new(status: SlavyankaStatus) -> Self {
        Self { handle: Mutex::new(None), status }
    }

    async fn spawn(&self, reverse_port: u16) {
        // If a task is already running, abort it first.
        self.kill().await;
        info!(reverse_port, "[sentry] spawning slavyanka (in-process task)");
        let status = self.status.clone();
        let h = tokio::spawn(async move {
            status.set_running();
            if let Err(e) = mirage_slavyanka::run(reverse_port).await {
                warn!(error = %e, "[sentry] slavyanka task ended with error");
                status.set_crashed();
            } else {
                info!("[sentry] slavyanka task ended cleanly");
                status.set_idle();
            }
        });
        let mut g = self.handle.lock().await;
        *g = Some(h);
    }

    async fn kill(&self) {
        let mut g = self.handle.lock().await;
        if let Some(h) = g.take() {
            h.abort();
            info!("[sentry] slavyanka task aborted");
        }
        self.status.set_idle();
    }
}

// ── WS subscription (one task per registered LM) ───────────────────────────
//
// Each registered LM gets its own WS subscription. All subscriptions feed
// into one shared mpsc<DispatchFrame> consumed by the controller below.

#[derive(Debug)]
enum DispatchFrame {
    SpawnSlavyanka { reverse_port: u16 },
    KillSlavyanka,
}

/// Keep a WS subscription to one (url, client_id) pair alive forever,
/// auto-reconnecting on failure. Each incoming server frame is parsed and
/// dispatched via `tx`.
async fn ws_subscribe_loop(
    reg: LmRegistration,
    tx: mpsc::Sender<DispatchFrame>,
) {
    // Build the wss:// subscribe URL. Original LM URL is `https://host:port`;
    // for WebSocket the scheme becomes `wss`, path is `/relay/clients/subscribe`,
    // query carries `client_id` so monitor routes per-client pushes.
    let ws_url = {
        let base = reg
            .url
            .trim_end_matches('/')
            .replace("https://", "wss://")
            .replace("http://", "ws://");
        format!("{}/relay/clients/subscribe?client_id={}", base, reg.client_id)
    };

    loop {
        match ws_once(&ws_url, &tx).await {
            Ok(()) => debug!(lm = %reg.url, "[sentry] ws connection closed cleanly, reconnecting in 5s"),
            Err(e) => debug!(lm = %reg.url, error = %e, "[sentry] ws error, reconnecting in 5s"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn ws_once(ws_url: &str, tx: &mpsc::Sender<DispatchFrame>) -> Result<()> {
    // Bearer auth on the upgrade request. Build a Request manually so we
    // can attach headers.
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = ws_url
        .into_client_request()
        .context("ws url -> request")?;
    let auth = format!("Bearer {}", MONITOR_API_KEY);
    req.headers_mut().insert(
        "Authorization",
        auth.parse().context("invalid auth header")?,
    );

    let (ws_stream, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .context("ws connect")?;
    debug!("[sentry] ws connected");

    let (mut write, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        let msg = msg.context("ws recv")?;
        match msg {
            WsMessage::Text(text) => {
                if let Some(frame) = parse_frame(&text) {
                    let _ = tx.send(frame).await;
                }
            }
            WsMessage::Ping(p) => {
                let _ = write.send(WsMessage::Pong(p)).await;
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

/// Parse one server-pushed JSON frame. Only the spawn / kill variants
/// matter for sentry — Config and unknown kinds are dropped silently.
fn parse_frame(text: &str) -> Option<DispatchFrame> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let kind = v.get("kind").and_then(|k| k.as_str())?;
    match kind {
        "debug_tunnel_open" => {
            let reverse_port = v.get("reverse_port").and_then(|p| p.as_u64())? as u16;
            Some(DispatchFrame::SpawnSlavyanka { reverse_port })
        }
        "debug_tunnel_close" => Some(DispatchFrame::KillSlavyanka),
        _ => None,
    }
}

/// Drain the dispatch mpsc and act on each frame.
async fn dispatch_loop(
    mut rx: mpsc::Receiver<DispatchFrame>,
    controller: Arc<SlavyankaController>,
) {
    while let Some(frame) = rx.recv().await {
        match frame {
            DispatchFrame::SpawnSlavyanka { reverse_port } => {
                info!(reverse_port, "[sentry] spawn-slavyanka requested");
                controller.spawn(reverse_port).await;
            }
            DispatchFrame::KillSlavyanka => {
                info!("[sentry] kill-slavyanka requested");
                controller.kill().await;
            }
        }
    }
}

// ── Public entry point ──────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    let state_dir = default_state_dir();
    info!(?state_dir, "[sentry] state dir");

    // Stable id minted once on this box, shared with every LM so all 9
    // registrations collapse to one logical client.
    let sentry_id = ensure_sentry_id(&state_dir)?;
    info!(sentry_id = %sentry_id, role = SENTRY_ROLE, "[sentry] identity");

    // Pre-populate DNS so reqwest bypasses the OS resolver. TLS cert
    // still validates against hostname (matches LM cert), no
    // danger_accept_invalid_certs needed.
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(10));
    for (host, ipport) in HOST_TO_IP {
        match ipport.parse::<std::net::SocketAddr>() {
            Ok(sa) => builder = builder.resolve(host, sa),
            Err(e) => warn!(host, ipport, error = %e, "[sentry] bad HOST_TO_IP"),
        }
    }
    let http = builder.build().context("build reqwest client")?;

    // Register on ALL reachable LMs. Sentry NEVER exits on register
    // failure — retry forever every 60s if NONE accept.
    let state = loop {
        match ensure_state(&http, &state_dir, &sentry_id).await {
            Ok(s) => break s,
            Err(e) => {
                warn!(error = %e, "[sentry] register failed on every LM — retrying in 60s");
                let mut sleep = Box::pin(tokio::time::sleep(Duration::from_secs(60)));
                let mut shutdown = Box::pin(tokio::signal::ctrl_c());
                tokio::select! {
                    _ = &mut sleep => continue,
                    _ = &mut shutdown => {
                        info!("[sentry] shutdown during register retry");
                        return Ok(());
                    }
                }
            }
        }
    };

    info!(
        count = state.registrations.len(),
        "[sentry] active on {} LMs: {:?}",
        state.registrations.len(),
        state.registrations.iter().map(|r| &r.url).collect::<Vec<_>>()
    );

    // Slavyanka as in-process tokio task.
    let status = SlavyankaStatus::new();
    let controller = Arc::new(SlavyankaController::new(status.clone()));

    // Multi-WS subscriber: one task per LM, feeding into a single mpsc.
    let (frame_tx, frame_rx) = mpsc::channel::<DispatchFrame>(64);
    let mut ws_tasks: Vec<JoinHandle<()>> = Vec::new();
    for reg in &state.registrations {
        let tx = frame_tx.clone();
        let reg = reg.clone();
        ws_tasks.push(tokio::spawn(async move {
            ws_subscribe_loop(reg, tx).await;
        }));
    }
    drop(frame_tx); // dispatch_loop exits when all senders drop.

    let dispatch_task = {
        let controller = Arc::clone(&controller);
        tokio::spawn(async move {
            dispatch_loop(frame_rx, controller).await;
        })
    };

    let hb_task = {
        let http = http.clone();
        let regs = state.registrations.clone();
        let status = status.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            heartbeat_loop(http, regs, status, started).await;
        })
    };

    info!("[sentry] ready — WS subscribed on {} LMs, heartbeating to all", state.registrations.len());

    let _ = tokio::signal::ctrl_c().await;
    info!("[sentry] Ctrl-C — shutting down");
    controller.kill().await;
    for h in ws_tasks { h.abort(); }
    dispatch_task.abort();
    hb_task.abort();
    Ok(())
}
