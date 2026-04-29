//! Background link to mirage2monitor — config push via WebSocket with HTTP poll fallback.
//!
//! [`MonitorLink`] registers with the monitor, then attempts a persistent
//! WebSocket connection to `/relay/clients/subscribe`.  When connected,
//! config updates are pushed by the server instantly.  If the WebSocket
//! upgrade fails or the connection drops, the link falls back to the existing
//! 30-second HTTP poll loop and retries the WebSocket upgrade every 5 minutes.
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
    monitor_url: String,
    /// Optional Bearer token forwarded in registration, heartbeat, and WS
    /// upgrade headers.
    monitor_api_key: Option<String>,
    client_id: Mutex<Option<String>>,
    http: reqwest::Client,
    config_version: AtomicU64,
    poll_interval_secs: AtomicU64,
    config_tx: watch::Sender<Option<MonitorConfig>>,
}

impl MonitorLink {
    /// Create a new [`MonitorLink`] and return the watch receiver.
    ///
    /// The link does not start until [`MonitorLink::spawn`] is called.
    /// `monitor_api_key` — if set, sent as `Authorization: Bearer <key>` on
    /// every request including the WebSocket upgrade handshake.
    pub fn new(
        monitor_url: String,
        monitor_api_key: Option<String>,
    ) -> (Arc<Self>, watch::Receiver<Option<MonitorConfig>>) {
        let (config_tx, config_rx) = watch::channel(None);

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest::Client construction is infallible with these settings");

        let link = Arc::new(Self {
            monitor_url,
            monitor_api_key,
            client_id: Mutex::new(None),
            http,
            config_version: AtomicU64::new(0),
            poll_interval_secs: AtomicU64::new(30),
            config_tx,
        });

        (link, config_rx)
    }

    /// Create an additional [`watch::Receiver`] subscribed to the same config
    /// channel.
    pub fn subscribe(&self) -> watch::Receiver<Option<MonitorConfig>> {
        self.config_tx.subscribe()
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
        // Register once up-front.
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

    /// Drive the WebSocket subscribe connection until it closes or errors.
    /// Returns `Ok(())` on clean close, `Err(...)` on connect/parse error.
    async fn run_ws_loop(&self) -> Result<(), String> {
        use futures::StreamExt;

        let ws_url = http_to_ws_url(&self.monitor_url);
        let subscribe_url = format!("{ws_url}/relay/clients/subscribe");

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
                    match serde_json::from_str::<ConfigResponse>(&text) {
                        Ok(cfg) => {
                            debug!(
                                version = cfg.version,
                                chains = cfg.chains.len(),
                                "monitor_link: WS config push received"
                            );
                            self.apply_config(cfg);
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
        };
        let _ = self.config_tx.send(Some(monitor_cfg));
    }

    async fn register(&self) {
        let url = format!("{}/relay/clients", self.monitor_url);
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
        let url = format!("{}/relay/clients/config", self.monitor_url);
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
                    warn!(status = %status, "monitor_link: config poll: unexpected status");
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

        let url = format!(
            "{}/relay/clients/{}/heartbeat",
            self.monitor_url, client_id
        );
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
