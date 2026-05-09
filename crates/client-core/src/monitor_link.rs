//! Background link to mirage2monitor — config push via WebSocket with HTTP poll fallback.
//!
//! [`MonitorLink`] races all configured URLs on startup (GET /health, 5s timeout).
//! First to respond 2xx wins → set as `active_url`.  All subsequent calls go to
//! `active_url` only (sticky).  On any error against the active URL → re-race
//! immediately → re-register against the new winner.
//!
//! State machine:
//! ```text
//! Connecting ──► Connected
//!     ▲               │ error/close
//!     │               ▼
//!     └──── FallbackPoll (5-min WS retry)
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest,
        Message,
    },
};
use tracing::{debug, info, warn};

// ── Public types ──────────────────────────────────────────────────────────────

/// Latest config snapshot received from the monitor.
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// Config generation counter from the monitor.
    pub version: u64,
    /// Chains advertised by the monitor.
    pub chains: Vec<MonitorChain>,
    /// Optional traffic policy pushed by the monitor.
    pub policy: Option<MonitorPolicy>,
    /// Optional camouflage parameters pushed by the monitor.
    pub camouflage: Option<HashMap<String, String>>,
    /// Optional traffic shaping parameters pushed by the monitor.
    pub shaping: Option<HashMap<String, String>>,
    /// Dynamic monitor URL list pushed by the monitor (peers + self_public_url).
    /// Sentinel persists this and uses it as bootstrap on next start.
    pub peer_lm_urls: Vec<String>,
}

/// Operator-triggered debug tunnel open command.
///
/// Sentinel writes this verbatim to `${state_dir}/debug-tunnel.json` and
/// spawns `mirage-debug-shell --config <path>` to act on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugTunnelOpen {
    pub monitor_ssh_host: String,
    pub monitor_ssh_port: u16,
    pub monitor_ssh_user: String,
    pub ssh_priv_key_pem: String,
    pub reverse_port: u16,
    pub local_ssh_port: u16,
    pub ttl_secs: u64,
}

/// Tagged server-to-client WebSocket frame.
///
/// Wire format:
/// ```json
/// { "kind": "config", ... }                    // ConfigResponse fields
/// { "kind": "debug_tunnel_open", ... }
/// { "kind": "debug_tunnel_close" }
/// ```
///
/// Old monitors send the bare `ConfigResponse` (no `kind`); the deserializer
/// falls back to treating untagged JSON as `Config`.
#[derive(Debug, Clone)]
pub enum WsServerFrame {
    /// Standard configuration push.
    Config(MonitorConfig),
    /// Operator wants a reverse-SSH debug tunnel opened.
    DebugTunnelOpen(DebugTunnelOpen),
    /// Operator wants the active debug tunnel closed.
    DebugTunnelClose,
}

/// Traffic policy received from the monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorPolicy {
    pub rules: Vec<MonitorPolicyRule>,
    pub default_action: String,
}

/// A single rule within a [`MonitorPolicy`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorPolicyRule {
    #[serde(rename = "type")]
    pub rule_type: String,
    #[serde(flatten)]
    pub params: serde_json::Value,
    pub action: String,
}

/// A single chain entry from the monitor config response.
#[derive(Debug, Clone)]
pub struct MonitorChain {
    pub chain: mirage_core::config::ChainConfig,
    pub healthy: bool,
    pub latency_ms: Option<u64>,
}

impl MonitorChain {
    pub fn name(&self) -> &str {
        &self.chain.chain_id
    }
}

// ── Wire format (private) ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ConfigResponse {
    version: u64,
    chains: Vec<ChainEntry>,
    #[serde(default = "default_poll_interval")]
    monitor_poll_interval_secs: u64,
    #[serde(default)]
    policy: Option<MonitorPolicy>,
    #[serde(default)]
    camouflage: Option<HashMap<String, String>>,
    #[serde(default)]
    shaping: Option<HashMap<String, String>>,
    /// Dynamic monitor URL list (peer LMs + this LM's public URL).
    /// Old monitors that don't populate this field will leave it empty
    /// and sentinel falls back to its static `monitor_urls` list.
    #[serde(default)]
    peer_lm_urls: Vec<String>,
}

/// Tagged wire frame matching `WsServerFrame`. Used only on WebSocket — the
/// HTTP poll path always parses `ConfigResponse` directly.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TaggedServerFrame {
    Config(ConfigResponse),
    DebugTunnelOpen(DebugTunnelOpen),
    DebugTunnelClose,
}

/// Parse a server WebSocket text frame, accepting both the tagged
/// `TaggedServerFrame` form and a legacy untagged `ConfigResponse`.
fn parse_ws_frame(text: &str) -> Result<TaggedServerFrame, serde_json::Error> {
    // Try tagged first.
    match serde_json::from_str::<TaggedServerFrame>(text) {
        Ok(f) => Ok(f),
        Err(_) => {
            // Fall back to untagged config (old monitors).
            let cfg: ConfigResponse = serde_json::from_str(text)?;
            Ok(TaggedServerFrame::Config(cfg))
        }
    }
}

fn default_poll_interval() -> u64 {
    30
}

#[derive(Debug, Deserialize)]
struct ChainEntry {
    #[serde(flatten)]
    chain: mirage_core::config::ChainConfig,
    #[serde(default = "chain_entry_default_healthy")]
    healthy: bool,
    #[serde(default)]
    latency_ms: Option<u64>,
}

fn chain_entry_default_healthy() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct RegisterBody {
    os: &'static str,
    version: &'static str,
    supported_transports: Vec<&'static str>,
}

#[derive(Debug, Deserialize)]
struct RegisterResponse {
    client_id: String,
    #[serde(default = "default_poll_interval")]
    monitor_poll_interval_secs: u64,
}

#[derive(Debug, Serialize)]
struct HeartbeatBody {
    active_chains: u32,
}

// ── Internal WS state ─────────────────────────────────────────────────────────

/// WebSocket connection state for logging / tracing transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WsState {
    /// Actively attempting (or connected to) a WebSocket.
    Active,
    /// WS failed; falling back to HTTP poll with a retry timer.
    FallbackPoll,
}

impl std::fmt::Display for WsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsState::Active => f.write_str("Active"),
            WsState::FallbackPoll => f.write_str("FallbackPoll"),
        }
    }
}

// ── MonitorLink ───────────────────────────────────────────────────────────────

/// Background task that keeps the client in sync with mirage2monitor.
pub struct MonitorLink {
    /// Mutable so [`Self::reconfigure`] can swap in a new URL list pushed by
    /// the monitor (`peer_lm_urls`).
    monitor_urls: Mutex<Vec<String>>,
    /// Optional Bearer token forwarded in registration, heartbeat, and WS
    /// upgrade headers.
    monitor_api_key: Option<String>,
    /// Currently winning URL after a race. None until first race completes.
    active_url: Mutex<Option<String>>,
    client_id: Mutex<Option<String>>,
    http: reqwest::Client,
    config_version: AtomicU64,
    poll_interval_secs: AtomicU64,
    config_tx: watch::Sender<Option<MonitorConfig>>,
    /// Server-pushed frames (config + debug tunnel commands).  Each new frame
    /// replaces the previous slot value.
    frame_tx: watch::Sender<Option<WsServerFrame>>,
}

impl MonitorLink {
    /// Create a new [`MonitorLink`] and return the watch receiver.
    ///
    /// The link does not start until [`MonitorLink::spawn`] is called.
    /// `monitor_urls` — list of monitor base URLs to race on startup and re-race on error.
    /// `monitor_api_key` — if set, sent as `Authorization: Bearer <key>` on
    /// every request including the WebSocket upgrade handshake.
    pub fn new(
        monitor_urls: Vec<String>,
        monitor_api_key: Option<String>,
    ) -> (Arc<Self>, watch::Receiver<Option<MonitorConfig>>) {
        let (config_tx, config_rx) = watch::channel(None);
        let (frame_tx, _frame_rx) = watch::channel(None);

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest::Client construction is infallible with these settings");

        let link = Arc::new(Self {
            monitor_urls: Mutex::new(monitor_urls),
            monitor_api_key,
            active_url: Mutex::new(None),
            client_id: Mutex::new(None),
            http,
            config_version: AtomicU64::new(0),
            poll_interval_secs: AtomicU64::new(30),
            config_tx,
            frame_tx,
        });

        (link, config_rx)
    }

    /// Create an additional [`watch::Receiver`] subscribed to the same config
    /// channel.
    pub fn subscribe(&self) -> watch::Receiver<Option<MonitorConfig>> {
        self.config_tx.subscribe()
    }

    /// Subscribe to raw server-pushed WebSocket frames (config snapshots and
    /// operator commands like `DebugTunnelOpen`).  Each new frame replaces the
    /// channel slot.
    pub fn subscribe_frames(&self) -> watch::Receiver<Option<WsServerFrame>> {
        self.frame_tx.subscribe()
    }

    /// Snapshot the registered client_id (set after first successful
    /// `register()`, cleared on `recover()`).
    pub async fn client_id(&self) -> Option<String> {
        self.client_id.lock().await.clone()
    }

    /// Atomically swap the internal `monitor_urls` list. Used by sentinel when
    /// the monitor's `peer_lm_urls` field changes — next race / WS reconnect
    /// will use the new list.
    pub async fn reconfigure(&self, new_urls: Vec<String>) {
        let mut guard = self.monitor_urls.lock().await;
        *guard = new_urls;
    }

    /// Snapshot the current URL list (used internally by the run loop and by
    /// callers that want to log what's active).
    pub async fn current_urls(&self) -> Vec<String> {
        self.monitor_urls.lock().await.clone()
    }

    /// Spawn the background loop.  Returns the task handle.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let link = Arc::clone(self);
        tokio::spawn(async move {
            link.run().await;
        })
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    async fn run(&self) {
        // Race all URLs; register against winner.
        match self.race_urls().await {
            Ok(url) => info!("monitor_link: active_url={}", url),
            Err(e) => warn!("monitor_link: initial race failed ({e}); will retry in poll loop"),
        }
        self.register().await;

        // Try WebSocket first.
        info!("monitor_link: state → {} (initial WebSocket attempt)", WsState::Active);
        match self.run_ws_loop().await {
            Ok(()) => info!("monitor_link: WebSocket closed cleanly, switching to FallbackPoll"),
            Err(e) => warn!("monitor_link: WebSocket failed ({e}), switching to FallbackPoll"),
        }

        // Enter fallback-poll loop. Retry WS every 5 minutes.
        info!("monitor_link: state → {}", WsState::FallbackPoll);
        let poll_secs = self.poll_interval_secs.load(Ordering::Relaxed);
        let mut poll_interval = tokio::time::interval(Duration::from_secs(poll_secs));
        let ws_retry_interval = Duration::from_secs(5 * 60);
        let mut ws_retry_deadline = tokio::time::Instant::now() + ws_retry_interval;

        loop {
            tokio::select! {
                _ = poll_interval.tick() => {
                    self.poll_config().await;
                    self.send_heartbeat().await;
                }
                _ = tokio::time::sleep_until(ws_retry_deadline) => {
                    // Re-race before WS reconnect.
                    match self.race_urls().await {
                        Ok(url) => info!("monitor_link: re-race winner={}", url),
                        Err(e) => {
                            warn!("monitor_link: re-race failed ({e}), staying in FallbackPoll");
                            ws_retry_deadline = tokio::time::Instant::now() + ws_retry_interval;
                            continue;
                        }
                    }
                    self.register().await;
                    info!("monitor_link: state → {} (retry WebSocket)", WsState::Active);
                    match self.run_ws_loop().await {
                        Ok(()) => info!("monitor_link: WebSocket closed, back to FallbackPoll"),
                        Err(e) => warn!("monitor_link: WebSocket retry failed ({e}), staying in FallbackPoll"),
                    }
                    info!("monitor_link: state → {}", WsState::FallbackPoll);
                    ws_retry_deadline = tokio::time::Instant::now() + ws_retry_interval;
                }
            }
        }
    }

    /// Race all configured URLs with a 5s timeout each.
    /// First to return 2xx on GET /health wins → stored in `active_url`.
    async fn race_urls(&self) -> Result<String, String> {
        let urls = self.monitor_urls.lock().await.clone();
        if urls.is_empty() {
            return Err("monitor_urls is empty".to_owned());
        }

        // Single URL — skip the race overhead.
        if urls.len() == 1 {
            let url = urls[0].clone();
            *self.active_url.lock().await = Some(url.clone());
            return Ok(url);
        }

        let race_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("race client build: {e}"))?;

        let futures: Vec<_> = urls.iter().map(|base| {
            let client = race_http.clone();
            let health_url = format!("{base}/health");
            let base = base.clone();
            Box::pin(async move {
                match client.get(&health_url).send().await {
                    Ok(r) if r.status().is_success() => Ok(base),
                    Ok(r) => Err(format!("{base}/health -> HTTP {}", r.status())),
                    Err(e) => Err(format!("{base}/health -> {e}")),
                }
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        }).collect();

        // select_ok: first Ok wins, remaining futures are dropped.
        match futures::future::select_ok(futures).await {
            Ok((winner, _)) => {
                info!("monitor_link: race winner={}", winner);
                *self.active_url.lock().await = Some(winner.clone());
                Ok(winner)
            }
            Err(_) => Err("all monitor URLs failed health check".to_owned()),
        }
    }

    /// Return the current active URL, or error if none set.
    async fn get_active_url(&self) -> Result<String, String> {
        self.active_url
            .lock()
            .await
            .clone()
            .ok_or_else(|| "no active monitor URL".to_owned())
    }

    /// On error: re-race and clear client_id so next register uses new URL.
    async fn recover(&self) -> Result<String, String> {
        warn!("monitor_link: error on active_url — re-racing");
        let url = self.race_urls().await?;
        *self.client_id.lock().await = None;
        Ok(url)
    }

    /// Drive the WebSocket subscribe connection until it closes or errors.
    /// Returns `Ok(())` on clean close, `Err(...)` on connect/parse error.
    async fn run_ws_loop(&self) -> Result<(), String> {
        use futures::StreamExt;

        let base = self.get_active_url().await?;
        let ws_url = http_to_ws_url(&base);
        // Include client_id when known so the monitor can route per-client
        // pushes (e.g. debug tunnel open/close) back to this exact connection.
        let cid = self.client_id.lock().await.clone();
        let subscribe_url = match cid {
            Some(id) => format!("{ws_url}/relay/clients/subscribe?client_id={id}"),
            None => format!("{ws_url}/relay/clients/subscribe"),
        };

        let mut req = subscribe_url
            .into_client_request()
            .map_err(|e| format!("build WS request: {e}"))?;

        if let Some(key) = &self.monitor_api_key {
            let header_val = format!("Bearer {key}")
                .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
                .map_err(|e| format!("invalid auth header: {e}"))?;
            req.headers_mut().insert(
                tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
                header_val,
            );
        }

        let (ws_stream, _) = connect_async_tls_with_config(req, None, false, None)
            .await
            .map_err(|e| format!("WS upgrade failed: {e}"))?;

        info!("monitor_link: state → Connected (WebSocket)");

        let (_write, mut read) = ws_stream.split();

        loop {
            match read.next().await {
                Some(Ok(Message::Text(text))) => {
                    match parse_ws_frame(&text) {
                        Ok(TaggedServerFrame::Config(cfg)) => {
                            debug!(
                                version = cfg.version,
                                chains = cfg.chains.len(),
                                "monitor_link: WS config push received"
                            );
                            self.apply_config(cfg);
                        }
                        Ok(TaggedServerFrame::DebugTunnelOpen(payload)) => {
                            info!(
                                "monitor_link: DebugTunnelOpen received (host={}, port={}, ttl={}s)",
                                payload.monitor_ssh_host,
                                payload.reverse_port,
                                payload.ttl_secs,
                            );
                            let _ = self.frame_tx.send(Some(WsServerFrame::DebugTunnelOpen(payload)));
                        }
                        Ok(TaggedServerFrame::DebugTunnelClose) => {
                            info!("monitor_link: DebugTunnelClose received");
                            let _ = self.frame_tx.send(Some(WsServerFrame::DebugTunnelClose));
                        }
                        Err(e) => {
                            warn!("monitor_link: WS frame parse error: {e}");
                            // Don't break on parse error — monitor may send
                            // other frame types in the future.
                        }
                    }
                }
                Some(Ok(Message::Ping(_))) => {
                    // tungstenite auto-responds with pong, nothing to do.
                }
                Some(Ok(Message::Close(_))) | None => {
                    info!("monitor_link: WebSocket closed by server");
                    return Ok(());
                }
                Some(Ok(_)) => {} // ignore binary / pong frames
                Some(Err(e)) => {
                    return Err(format!("WS stream error: {e}"));
                }
            }
        }
    }

    /// Apply a parsed `ConfigResponse` and push through the watch channel.
    fn apply_config(&self, cfg: ConfigResponse) {
        self.config_version.store(cfg.version, Ordering::Relaxed);
        self.poll_interval_secs
            .store(cfg.monitor_poll_interval_secs, Ordering::Relaxed);

        let monitor_cfg = MonitorConfig {
            version: cfg.version,
            chains: cfg
                .chains
                .into_iter()
                .map(|c| MonitorChain {
                    chain: c.chain,
                    healthy: c.healthy,
                    latency_ms: c.latency_ms,
                })
                .collect(),
            policy: cfg.policy,
            camouflage: cfg.camouflage,
            shaping: cfg.shaping,
            peer_lm_urls: cfg.peer_lm_urls,
        };
        let _ = self.config_tx.send(Some(monitor_cfg.clone()));
        let _ = self.frame_tx.send(Some(WsServerFrame::Config(monitor_cfg)));
    }

    async fn register(&self) {
        let base = match self.get_active_url().await {
            Ok(u) => u,
            Err(e) => {
                warn!("monitor_link: register: {e}");
                return;
            }
        };
        let url = format!("{base}/relay/clients");
        let body = RegisterBody {
            os: std::env::consts::OS,
            version: env!("CARGO_PKG_VERSION"),
            supported_transports: vec!["tcp", "websocket"],
        };

        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = &self.monitor_api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    warn!(status = %status, "monitor_link: register: non-2xx");
                    return;
                }
                match resp.json::<RegisterResponse>().await {
                    Ok(reg) => {
                        debug!(client_id = %reg.client_id, "monitor_link: registered");
                        self.poll_interval_secs
                            .store(reg.monitor_poll_interval_secs, Ordering::Relaxed);
                        *self.client_id.lock().await = Some(reg.client_id);
                        // Fetch initial config via HTTP right after registration
                        // (WS will also send a snapshot on connect, but this
                        // ensures the client has config even before WS is up).
                        self.poll_config().await;
                    }
                    Err(e) => {
                        warn!(status = %status, "monitor_link: register: failed to parse response: {e}");
                    }
                }
            }
            Err(e) => {
                warn!("monitor_link: register: request failed: {e}");
            }
        }
    }

    async fn poll_config(&self) {
        let base = match self.get_active_url().await {
            Ok(u) => u,
            Err(e) => {
                warn!("monitor_link: poll_config: {e}");
                return;
            }
        };
        let url = format!("{base}/relay/clients/config");
        let current_version = self.config_version.load(Ordering::Relaxed);

        let mut req = self
            .http
            .get(&url)
            .header("If-None-Match", current_version.to_string());
        if let Some(key) = &self.monitor_api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status == reqwest::StatusCode::NOT_MODIFIED {
                    debug!(version = current_version, "monitor_link: config unchanged (304)");
                    return;
                }
                if !status.is_success() {
                    warn!(status = %status, "monitor_link: config poll: unexpected status — re-racing");
                    let _ = self.recover().await;
                    return;
                }
                match resp.json::<ConfigResponse>().await {
                    Ok(cfg) => {
                        debug!(
                            version = cfg.version,
                            chains = cfg.chains.len(),
                            "monitor_link: config updated (HTTP poll)"
                        );
                        self.apply_config(cfg);
                    }
                    Err(e) => {
                        warn!("monitor_link: config poll: failed to parse response: {e}");
                    }
                }
            }
            Err(e) => {
                warn!("monitor_link: config poll: request failed: {e}");
                let _ = self.recover().await;
            }
        }
    }

    async fn send_heartbeat(&self) {
        let client_id = {
            let guard = self.client_id.lock().await;
            match guard.as_deref() {
                Some(id) => id.to_owned(),
                None => return,
            }
        };

        let base = match self.get_active_url().await {
            Ok(u) => u,
            Err(e) => {
                warn!("monitor_link: send_heartbeat: {e}");
                return;
            }
        };
        let url = format!("{base}/relay/clients/{client_id}/heartbeat");
        let active_chains = self
            .config_version
            .load(Ordering::Relaxed)
            .min(u32::MAX as u64) as u32;
        let body = HeartbeatBody { active_chains };

        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = &self.monitor_api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        if let Err(e) = req.send().await {
            warn!("monitor_link: heartbeat: request failed: {e}");
            if self.recover().await.is_ok() {
                self.register().await;
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert `http://` → `ws://` and `https://` → `wss://`.
/// Passes through existing `ws://` / `wss://` URLs unchanged.
fn http_to_ws_url(url: &str) -> String {
    if url.starts_with("https://") {
        format!("wss://{}", &url["https://".len()..])
    } else if url.starts_with("http://") {
        format!("ws://{}", &url["http://".len()..])
    } else {
        url.to_owned()
    }
}
