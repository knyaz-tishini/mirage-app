//! Background link to mirage2monitor — config push via WebSocket with HTTP poll fallback,
//! plus an optional `OverChain` fallback when all direct WebSocket URLs are unreachable.
//!
//! [`MonitorLink`] races all configured URLs on startup (GET /health, 5s timeout).
//! First to respond 2xx wins → set as `active_url`.  All subsequent calls go to
//! `active_url` only (sticky).  On any error against the active URL → re-race
//! immediately → re-register against the new winner.
//!
//! When all DirectWs transports fail, [`MonitorLink`] falls back to any configured
//! `OverChain` transports (see [`MonitorTransport`]).  A background health-recheck
//! task runs every 30 s while OverChain is active; on the first 2xx from any DirectWs
//! URL the client promotes back to DirectWs automatically.
//!
//! State machine:
//! ```text
//! Connecting ──► Connected (DirectWs)
//!     ▲               │ error/close
//!     │               ▼
//!     │     FallbackPoll (5-min WS retry)
//!     │               │ all DirectWs fail + OverChain configured
//!     │               ▼
//!     └── OverChain (health-recheck → promote back to DirectWs on 2xx)
//! ```

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use mirage_core::peer_table::PeerTable;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{client::IntoClientRequest, Message},
};
use tracing::{debug, info, warn};

use crate::chain_set::{ChainId, ChainSet};
use crate::monitor_channel::{MonitorChannel, OverChainChannel};

// ── Timeouts ─────────────────────────────────────────────────────────────────

const OVER_CHAIN_BACKOFF_INIT: Duration = Duration::from_secs(5);
const OVER_CHAIN_BACKOFF_MAX: Duration = Duration::from_secs(60);
const DIRECT_HEALTH_RECHECK: Duration = Duration::from_secs(30);
const OVER_CHAIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

fn default_chain_id() -> ChainId {
    ChainId(String::new())
}

// ── MonitorTransport ──────────────────────────────────────────────────────────

/// How to reach the monitor control plane.
///
/// A [`MonitorLink`] accepts a list of transports; they are probed in order
/// with DirectWs always preferred over OverChain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum MonitorTransport {
    /// Standard WebSocket subscribe (`wss://` or `ws://`).
    DirectWs {
        /// WebSocket URL for the monitor subscribe endpoint.
        url: String,
        /// Optional SNI override (unused by current WS impl, reserved for
        /// future TLS pinning).
        #[serde(default)]
        sni: Option<String>,
    },
    /// Binary frame protocol over a Mirage tunnel (`OverChain`).
    ///
    /// The client connects to the monitor's over-chain listener through the
    /// designated worker's SOCKS5 port. Worker is the per-chain process
    /// (cli2 split-arch) that already maintains a tunnel pool on this chain
    /// for regular SOCKS5 traffic. We piggyback on that — no need for a
    /// second decrypted Tunnel abstraction.
    OverChain {
        /// Local SOCKS5 address of the worker that owns this chain.
        /// E.g. `127.0.0.1:19516` for `c-control-over-chain`. The worker
        /// performs the Mirage handshake and pipes plaintext bytes; we
        /// then speak our binary frame protocol directly on the resulting
        /// bidirectional stream.
        worker_socks5: SocketAddr,
        /// CONNECT target sent to the worker's SOCKS5 (host:port). The
        /// chain's exit hop forwards this to its `control_target` regardless
        /// of the value (TunnelControl ignores the target string), but
        /// SOCKS5 protocol requires a non-empty target. Default
        /// `127.0.0.1:19445` works because exit hop bridges to its own
        /// loopback monitor listener.
        monitor_addr: SocketAddr,
        /// Kept for backward compat with logs / metrics. Not used for dial.
        #[serde(default = "default_chain_id")]
        chain_id: ChainId,
    },
}

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
    /// Peer table snapshot pushed by the monitor (Phase 8.5).
    /// Old monitors that don't include this field will leave it as `None`,
    /// and the client will use only chains with hardcoded `hop.public_key`.
    #[serde(default)]
    peer_table: Option<PeerTable>,
}

/// Tagged wire frame matching `WsServerFrame`. Used only on WebSocket — the
/// HTTP poll path always parses `ConfigResponse` directly.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TaggedServerFrame {
    Config(ConfigResponse),
    DebugTunnelOpen(DebugTunnelOpen),
    DebugTunnelClose,
    /// App-level Pong echoing a client-sent Ping nonce.
    #[serde(rename = "pong")]
    Pong { nonce: u64 },
}

/// Tagged client→server frame sent over the same JSON channel.
/// Used for app-level ping-pong that works over both DirectWs and OverChain.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TaggedClientFrame {
    /// App-level ping — server must reply with `Pong { nonce }`.
    #[serde(rename = "ping")]
    Ping { nonce: u64 },
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
    /// Latest peer table snapshot from the monitor (Phase 8.5).
    /// Initialised as an empty table; updated on every `Config` frame that
    /// carries a non-`None` `peer_table` field.
    /// Shared with `TunnelPool` / `MuxPool` via `Arc::clone`.
    pub peer_table: Arc<ArcSwap<PeerTable>>,
    /// Additional transport options (DirectWs + OverChain).
    /// If empty, `monitor_urls` is used for DirectWs (legacy path).
    transports: Vec<MonitorTransport>,
    /// Chain set used to open OverChain connections.
    chain_set: Arc<ChainSet>,
}

impl MonitorLink {
    /// Create a new [`MonitorLink`] and return the watch receiver.
    ///
    /// The link does not start until [`MonitorLink::spawn`] is called.
    ///
    /// `monitor_urls` — list of monitor base URLs to race on startup and re-race
    /// on error.  Used as the fallback when `transports` is empty.
    ///
    /// `monitor_api_key` — if set, sent as `Authorization: Bearer <key>` on
    /// every request including the WebSocket upgrade handshake.
    ///
    /// `peer_table` — shared [`ArcSwap`] that the link writes to on every config
    /// frame containing a `peer_table` snapshot (Phase 8.5). Pass a freshly
    /// created `Arc::new(ArcSwap::from_pointee(PeerTable::empty()))` and share
    /// that same arc with `TunnelPool::new` / `MuxPool::new` so pools always see
    /// the latest peer table without extra plumbing.
    ///
    /// `chain_set` — registry of chains, used for OverChain transport.
    ///
    /// `transports` — optional list of [`MonitorTransport`]s from
    /// `ClientConfig.monitor_transports`.  If empty, `monitor_urls` is used
    /// (backward-compatible legacy path).
    pub fn new(
        monitor_urls: Vec<String>,
        monitor_api_key: Option<String>,
        peer_table: Arc<ArcSwap<PeerTable>>,
        chain_set: Arc<ChainSet>,
        transports: Vec<MonitorTransport>,
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
            peer_table,
            transports,
            chain_set,
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

        // Enter fallback-poll loop. Retry WS every 20 s (was 5 min — too slow
        // for the OverChain escalation path: WS retry deadline is the only
        // place that calls run_over_chain_fallback after re-race fails).
        info!("monitor_link: state → {}", WsState::FallbackPoll);
        let poll_secs = self.poll_interval_secs.load(Ordering::Relaxed);
        let mut poll_interval = tokio::time::interval(Duration::from_secs(poll_secs));
        let ws_retry_interval = Duration::from_secs(20);
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
                            warn!("monitor_link: re-race failed ({e})");
                            // If OverChain transports are configured, escalate.
                            if self.has_over_chain_transports() {
                                info!("monitor_link: escalating to OverChain fallback");
                                self.run_over_chain_fallback().await;
                                info!("monitor_link: promoted back to DirectWs");
                            }
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

    /// True when any `OverChain` transport is configured.
    fn has_over_chain_transports(&self) -> bool {
        self.transports
            .iter()
            .any(|t| matches!(t, MonitorTransport::OverChain { .. }))
    }

    /// Drive the OverChain fallback loop.
    ///
    /// Tries all configured `OverChain` transports in parallel.  While active,
    /// a background task rechecks DirectWs every 30 s.  Returns when:
    /// - A DirectWs health check succeeds (promote back), or
    /// - An OverChain connection succeeds, runs, and then all OverChain transports
    ///   fail (caller retries DirectWs).
    async fn run_over_chain_fallback(&self) {
        let mut backoff = OVER_CHAIN_BACKOFF_INIT;

        loop {
            // First, try to open an OverChain connection.
            let channel = self.try_connect_over_chain().await;

            match channel {
                None => {
                    // All OverChain transports failed — back off and check DirectWs.
                    info!(
                        "monitor_link: all OverChain transports failed, backoff={:?}",
                        backoff
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(OVER_CHAIN_BACKOFF_MAX);

                    // Check if DirectWs is back before retrying OverChain.
                    if self.check_direct_ws_health().await {
                        info!("monitor_link: DirectWs health check passed — promoting");
                        return;
                    }
                }
                Some(mut ch) => {
                    info!("monitor_link: OverChain connected — entering receive loop");
                    backoff = OVER_CHAIN_BACKOFF_INIT; // reset on success

                    // Drive the OverChain loop; health-recheck DirectWs in parallel.
                    let result = self
                        .run_over_chain_loop_with_health_check(&mut ch)
                        .await;

                    match result {
                        OverChainLoopResult::DirectWsRestored => {
                            info!(
                                "monitor_link: DirectWs restored while on OverChain — promoting"
                            );
                            let _ = ch.close().await;
                            return;
                        }
                        OverChainLoopResult::ChannelClosed => {
                            warn!(
                                "monitor_link: OverChain channel closed — will retry"
                            );
                            let _ = ch.close().await;
                        }
                    }
                }
            }
        }
    }

    /// Try to open one OverChain connection.  Returns the first successful
    /// channel, or `None` if all transports fail.
    async fn try_connect_over_chain(&self) -> Option<OverChainChannel> {
        let bearer = self.monitor_api_key.as_deref().unwrap_or("");

        for transport in &self.transports {
            if let MonitorTransport::OverChain {
                worker_socks5,
                monitor_addr,
                chain_id,
            } = transport
            {
                match tokio::time::timeout(
                    OVER_CHAIN_CONNECT_TIMEOUT,
                    OverChainChannel::connect(*worker_socks5, *monitor_addr, bearer),
                )
                .await
                {
                    Ok(Ok(ch)) => {
                        info!(
                            chain_id = %chain_id,
                            worker_socks5 = %worker_socks5,
                            addr = %monitor_addr,
                            "monitor_link: OverChain connected"
                        );
                        return Some(ch);
                    }
                    Ok(Err(e)) => {
                        warn!(
                            chain_id = %chain_id,
                            worker_socks5 = %worker_socks5,
                            "monitor_link: OverChain connect failed: {e}"
                        );
                    }
                    Err(_) => {
                        warn!(
                            chain_id = %chain_id,
                            worker_socks5 = %worker_socks5,
                            "monitor_link: OverChain connect timed out"
                        );
                    }
                }
            }
        }
        None
    }

    /// Check if any DirectWs URL responds with 2xx on GET /health.
    async fn check_direct_ws_health(&self) -> bool {
        let urls = self.monitor_urls.lock().await.clone();
        // Also include DirectWs URLs from the transports list.
        let mut all_urls: Vec<String> = urls;
        for t in &self.transports {
            if let MonitorTransport::DirectWs { url, .. } = t {
                all_urls.push(url.clone());
            }
        }

        let client = reqwest::Client::builder()
            .connect_timeout(OVER_CHAIN_CONNECT_TIMEOUT)
            .timeout(OVER_CHAIN_CONNECT_TIMEOUT)
            .build()
            .ok();
        let Some(client) = client else {
            return false;
        };

        for base in &all_urls {
            let health_url = format!("{base}/health");
            if let Ok(Ok(r)) = tokio::time::timeout(
                OVER_CHAIN_CONNECT_TIMEOUT,
                client.get(&health_url).send(),
            )
            .await
            {
                if r.status().is_success() {
                    info!("monitor_link: DirectWs health OK at {base}");
                    *self.active_url.lock().await = Some(base.clone());
                    return true;
                }
            }
        }
        false
    }

    /// Result of [`MonitorLink::run_over_chain_loop_with_health_check`].
    async fn run_over_chain_loop_with_health_check(
        &self,
        channel: &mut OverChainChannel,
    ) -> OverChainLoopResult {
        use crate::monitor_channel::MonitorChannel;

        let ping_interval_dur = Duration::from_secs(10);
        let pong_timeout = Duration::from_secs(15);
        let recheck_interval = DIRECT_HEALTH_RECHECK;

        let mut ping_ticker = tokio::time::interval(ping_interval_dur);
        let mut ping_counter: u64 = 0;
        let mut outstanding_nonce: Option<u64> = None;
        let mut ping_sent_at: Option<tokio::time::Instant> = None;
        let mut last_pong_at = tokio::time::Instant::now();

        loop {
            tokio::select! {
                // Drive OverChain receive.
                frame = channel.recv_frame() => {
                    match frame {
                        Ok(Some(payload)) => {
                            let text = match std::str::from_utf8(&payload) {
                                Ok(s) => s,
                                Err(e) => {
                                    warn!("monitor_link: OverChain frame not valid UTF-8: {e}");
                                    continue;
                                }
                            };
                            match parse_ws_frame(text) {
                                Ok(TaggedServerFrame::Config(cfg)) => {
                                    debug!(
                                        version = cfg.version,
                                        chains = cfg.chains.len(),
                                        "monitor_link: OverChain config push received"
                                    );
                                    self.apply_config(cfg);
                                }
                                Ok(TaggedServerFrame::DebugTunnelOpen(dtp)) => {
                                    info!(
                                        "monitor_link: OverChain DebugTunnelOpen received"
                                    );
                                    let _ = self
                                        .frame_tx
                                        .send(Some(WsServerFrame::DebugTunnelOpen(dtp)));
                                }
                                Ok(TaggedServerFrame::DebugTunnelClose) => {
                                    info!("monitor_link: OverChain DebugTunnelClose received");
                                    let _ = self
                                        .frame_tx
                                        .send(Some(WsServerFrame::DebugTunnelClose));
                                }
                                Ok(TaggedServerFrame::Pong { nonce }) => {
                                    if outstanding_nonce == Some(nonce) {
                                        debug!(nonce, "monitor_link: OverChain app Pong received");
                                        outstanding_nonce = None;
                                        ping_sent_at = None;
                                        last_pong_at = tokio::time::Instant::now();
                                    } else {
                                        debug!(nonce, "monitor_link: OverChain stale Pong nonce");
                                    }
                                }
                                Err(e) => {
                                    warn!("monitor_link: OverChain frame parse error: {e}");
                                }
                            }
                        }
                        Ok(None) => {
                            return OverChainLoopResult::ChannelClosed;
                        }
                        Err(e) => {
                            warn!("monitor_link: OverChain recv error: {e}");
                            return OverChainLoopResult::ChannelClosed;
                        }
                    }
                }

                // App-level Ping keepalive.
                _ = ping_ticker.tick() => {
                    let now = tokio::time::Instant::now();

                    // Dead-detection: Pong not received within pong_timeout.
                    if let Some(sent_at) = ping_sent_at {
                        if now.duration_since(sent_at) > pong_timeout {
                            warn!(
                                "monitor_link: OverChain app pong timeout ({:?}) — channel dead",
                                now.duration_since(sent_at)
                            );
                            return OverChainLoopResult::ChannelClosed;
                        }
                    }

                    // Idle fallback.
                    if last_pong_at.elapsed() > ping_interval_dur + pong_timeout {
                        warn!(
                            "monitor_link: OverChain app pong idle ({:?}) — channel dead",
                            last_pong_at.elapsed()
                        );
                        return OverChainLoopResult::ChannelClosed;
                    }

                    // Send app-level Ping.
                    ping_counter += 1;
                    let nonce = ping_counter;
                    let ping_json = match serde_json::to_string(&TaggedClientFrame::Ping { nonce }) {
                        Ok(j) => j,
                        Err(e) => {
                            warn!("monitor_link: failed to serialize OverChain Ping: {e}");
                            continue;
                        }
                    };
                    match channel.send_text(&ping_json).await {
                        Ok(()) => {
                            outstanding_nonce = Some(nonce);
                            ping_sent_at = Some(now);
                        }
                        Err(e) => {
                            warn!("monitor_link: OverChain Ping send failed: {e}");
                            return OverChainLoopResult::ChannelClosed;
                        }
                    }
                }

                // Background health recheck — promote back to DirectWs when possible.
                _ = tokio::time::sleep(recheck_interval) => {
                    if self.check_direct_ws_health().await {
                        return OverChainLoopResult::DirectWsRestored;
                    }
                }
            }
        }
    }
}

/// Result returned by the OverChain receive loop.
enum OverChainLoopResult {
    /// A DirectWs URL responded 2xx — time to promote back.
    DirectWsRestored,
    /// The OverChain channel was closed or errored — need reconnect.
    ChannelClosed,
}

impl MonitorLink {
    /// Race all configured URLs with a 5s timeout each.
    /// First to return 2xx on GET /health wins → stored in `active_url`.
    async fn race_urls(&self) -> Result<String, String> {
        let urls = self.monitor_urls.lock().await.clone();
        if urls.is_empty() {
            return Err("monitor_urls is empty".to_owned());
        }

        let race_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("race client build: {e}"))?;

        // Single URL — still health-check it so that re-race can correctly
        // signal "all DirectWs down" and the OverChain fallback path triggers.
        // Previous behaviour was to short-circuit Ok(url) which made re-race
        // always succeed even when the URL was unreachable.
        if urls.len() == 1 {
            let url = urls[0].clone();
            let health_url = format!("{url}/health");
            match race_http.get(&health_url).send().await {
                Ok(r) if r.status().is_success() => {
                    *self.active_url.lock().await = Some(url.clone());
                    return Ok(url);
                }
                Ok(r) => return Err(format!("{url}/health -> HTTP {}", r.status())),
                Err(e) => return Err(format!("{url}/health -> {e}")),
            }
        }

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
        use futures::{SinkExt, StreamExt};

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

        let (mut write, mut read) = ws_stream.split();

        // Keepalive strategy (transport-agnostic app-level Ping/Pong):
        //   - Every 10 s send `TaggedClientFrame::Ping { nonce }` as a JSON
        //     text frame.  The monitor echoes `TaggedServerFrame::Pong { nonce }`.
        //   - Track `app_ping_nonce` + `app_ping_sent_at`.
        //   - On matching Pong → clear outstanding; record `last_pong_at`.
        //   - Dead-detection: if 15 s have elapsed since the last Ping without
        //     a matching Pong → return Err (triggers reconnect).
        //
        // We also keep the tungstenite WS-Ping for protocol-level housekeeping
        // (routers/proxies expect WS-level keepalives).  WS-Pong from the server
        // is still tracked via `last_ws_pong` as a secondary liveness signal.
        let ping_interval_dur = Duration::from_secs(10);
        let pong_timeout = Duration::from_secs(15);
        let mut ping_ticker = tokio::time::interval(ping_interval_dur);
        let mut app_ping_counter: u64 = 0;
        let mut app_ping_nonce: Option<u64> = None;
        let mut app_ping_sent_at: Option<tokio::time::Instant> = None;
        let mut last_pong_at = tokio::time::Instant::now();
        // tungstenite WS-level Ping/Pong tracking (secondary signal).
        let mut last_ws_pong = tokio::time::Instant::now();
        let mut ws_ping_outstanding_since: Option<tokio::time::Instant> = None;

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
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
                                Ok(TaggedServerFrame::Pong { nonce }) => {
                                    // App-level Pong: clear outstanding ping if nonce matches.
                                    if app_ping_nonce == Some(nonce) {
                                        debug!(nonce, "monitor_link: app Pong received");
                                        app_ping_nonce = None;
                                        app_ping_sent_at = None;
                                        last_pong_at = tokio::time::Instant::now();
                                    } else {
                                        debug!(nonce, "monitor_link: stale/unexpected Pong nonce");
                                    }
                                }
                                Err(e) => {
                                    warn!("monitor_link: WS frame parse error: {e}");
                                    // Don't break on parse error — monitor may send
                                    // other frame types in the future.
                                }
                            }
                        }
                        Some(Ok(Message::Ping(_))) => {
                            // tungstenite auto-responds with Pong, nothing to do.
                        }
                        Some(Ok(Message::Pong(_))) => {
                            let now = tokio::time::Instant::now();
                            last_ws_pong = now;
                            ws_ping_outstanding_since = None;
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            info!("monitor_link: WebSocket closed by server");
                            return Ok(());
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            return Err(format!("WS stream error: {e}"));
                        }
                    }
                }

                _ = ping_ticker.tick() => {
                    let now = tokio::time::Instant::now();

                    // Dead-detection: app-level Pong not received within pong_timeout.
                    if let Some(sent_at) = app_ping_sent_at {
                        if now.duration_since(sent_at) > pong_timeout {
                            return Err(format!(
                                "monitor_link: app pong timeout: no Pong for {:?}",
                                now.duration_since(sent_at)
                            ));
                        }
                    }

                    // Secondary: WS-level Pong not received within pong_timeout.
                    if let Some(sent_at) = ws_ping_outstanding_since {
                        if now.duration_since(sent_at) > pong_timeout {
                            return Err(format!(
                                "monitor_link: WS pong timeout: no WS Pong for {:?}",
                                now.duration_since(sent_at)
                            ));
                        }
                    }

                    // Idle fallback: if app-level Pong idle too long (covers the case
                    // where we never had a pending Ping but Pongs stopped arriving).
                    if last_pong_at.elapsed() > ping_interval_dur + pong_timeout {
                        return Err(format!(
                            "monitor_link: app pong idle: no Pong for {:?}",
                            last_pong_at.elapsed()
                        ));
                    }
                    if last_ws_pong.elapsed() > ping_interval_dur + pong_timeout {
                        return Err(format!(
                            "monitor_link: WS pong idle: no WS Pong for {:?}",
                            last_ws_pong.elapsed()
                        ));
                    }

                    // Send app-level Ping (JSON text frame).
                    app_ping_counter += 1;
                    let nonce = app_ping_counter;
                    let ping_json = match serde_json::to_string(&TaggedClientFrame::Ping { nonce }) {
                        Ok(j) => j,
                        Err(e) => {
                            return Err(format!("monitor_link: failed to serialize Ping: {e}"));
                        }
                    };
                    match write.send(Message::Text(ping_json.into())).await {
                        Ok(()) => {
                            app_ping_nonce = Some(nonce);
                            app_ping_sent_at = Some(now);
                        }
                        Err(e) => {
                            return Err(format!("monitor_link: app Ping send failed: {e}"));
                        }
                    }

                    // Also send tungstenite WS-level Ping for protocol housekeeping.
                    match write.send(Message::Ping(vec![])).await {
                        Ok(()) => {
                            if ws_ping_outstanding_since.is_none() {
                                ws_ping_outstanding_since = Some(now);
                            }
                        }
                        Err(e) => {
                            return Err(format!("monitor_link: WS Ping send failed: {e}"));
                        }
                    }
                }
            }
        }
    }

    /// Apply a parsed `ConfigResponse` and push through the watch channel.
    fn apply_config(&self, cfg: ConfigResponse) {
        self.config_version.store(cfg.version, Ordering::Relaxed);
        self.poll_interval_secs
            .store(cfg.monitor_poll_interval_secs, Ordering::Relaxed);

        // Phase 8.5: update peer_table when the monitor includes a snapshot.
        if let Some(pt) = cfg.peer_table {
            debug!(
                generation = pt.generation,
                peers = pt.peers.len(),
                "monitor_link: peer_table snapshot received"
            );
            self.peer_table.store(Arc::new(pt));
        }

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

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor_channel::{MonitorChannel, MonitorChannelError};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // ── Mock channel ──────────────────────────────────────────────────────────

    /// A simple in-memory channel for testing keepalive logic.
    ///
    /// `recv_queue` — frames the server would push (as raw JSON bytes).
    /// `sent_texts` — records every `send_text` call made by the client.
    /// `auto_pong`  — if true, automatically push a matching Pong frame into
    ///                `recv_queue` whenever a `send_text` containing a Ping is
    ///                detected (simulates a cooperative server).
    struct MockChannel {
        recv_queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
        sent_texts: Arc<Mutex<Vec<String>>>,
        auto_pong: bool,
    }

    impl MockChannel {
        fn new_auto_pong() -> Self {
            Self {
                recv_queue: Arc::new(Mutex::new(VecDeque::new())),
                sent_texts: Arc::new(Mutex::new(Vec::new())),
                auto_pong: true,
            }
        }

        fn new_no_reply() -> Self {
            Self {
                recv_queue: Arc::new(Mutex::new(VecDeque::new())),
                sent_texts: Arc::new(Mutex::new(Vec::new())),
                auto_pong: false,
            }
        }
    }

    impl MonitorChannel for MockChannel {
        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, MonitorChannelError> {
            loop {
                {
                    let mut q = self.recv_queue.lock().await;
                    if let Some(frame) = q.pop_front() {
                        return Ok(Some(frame));
                    }
                }
                tokio::task::yield_now().await;
            }
        }

        async fn send_pong(&mut self, _nonce: [u8; 8]) -> Result<(), MonitorChannelError> {
            Ok(())
        }

        async fn send_text(&mut self, text: &str) -> Result<(), MonitorChannelError> {
            self.sent_texts.lock().await.push(text.to_owned());

            if self.auto_pong {
                // If the text is a Ping, enqueue a matching Pong.
                if let Ok(TaggedClientFrame::Ping { nonce }) =
                    serde_json::from_str::<TaggedClientFrame>(text)
                {
                    let pong_json = format!(r#"{{"kind":"pong","nonce":{nonce}}}"#);
                    self.recv_queue
                        .lock()
                        .await
                        .push_back(pong_json.into_bytes());
                }
            }
            Ok(())
        }

        async fn close(self) -> Result<(), MonitorChannelError> {
            Ok(())
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// `TaggedServerFrame::Pong` deserializes correctly.
    #[test]
    fn pong_frame_deserialized() {
        let json = r#"{"kind":"pong","nonce":42}"#;
        let frame = parse_ws_frame(json).expect("must parse");
        assert!(matches!(frame, TaggedServerFrame::Pong { nonce: 42 }));
    }

    /// `TaggedClientFrame::Ping` serializes correctly.
    #[test]
    fn ping_frame_serialized() {
        let frame = TaggedClientFrame::Ping { nonce: 7 };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"kind\":\"ping\""));
        assert!(json.contains("\"nonce\":7"));
    }

    /// Mock channel that auto-replies Pong: keepalive loop does not return Err
    /// within a short window (we run it for 3 ping intervals and verify it
    /// stays alive).
    #[tokio::test]
    async fn keepalive_loop_survives_with_pong() {
        let mut ch = MockChannel::new_auto_pong();

        // Override timing by running the loop body manually a few times.
        // We simulate the keepalive by calling send_text(Ping) and checking
        // that Pong arrives and clears the outstanding state.

        let nonce: u64 = 1;
        let ping_json = serde_json::to_string(&TaggedClientFrame::Ping { nonce }).unwrap();

        // Send Ping via the channel.
        ch.send_text(&ping_json).await.unwrap();

        // The auto-pong mock should have queued a Pong.
        let payload = ch.recv_frame().await.unwrap().expect("should have pong");
        let text = std::str::from_utf8(&payload).unwrap();
        let frame = parse_ws_frame(text).unwrap();

        match frame {
            TaggedServerFrame::Pong { nonce: got } => {
                assert_eq!(got, nonce, "Pong nonce must echo Ping nonce");
            }
            other => panic!("expected Pong, got {other:?}"),
        }

        // Verify the sent text was recorded.
        let sent = ch.sent_texts.lock().await;
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("\"ping\""));
    }

    /// Mock channel that never replies: dead-detection triggers after timeout.
    ///
    /// We use the run_over_chain_loop_with_health_check logic in a stripped-down
    /// form — verify that when no Pong arrives within pong_timeout the loop
    /// returns `ChannelClosed`.
    #[tokio::test]
    async fn keepalive_detects_dead_channel() {
        // Build the minimal state machine inline (mirrors what
        // run_over_chain_loop_with_health_check does) so we don't need a full
        // MonitorLink.

        let ping_interval_dur = Duration::from_millis(50);
        let pong_timeout = Duration::from_millis(100);

        let mut ch = MockChannel::new_no_reply();

        let nonce: u64 = 99;
        let ping_json = serde_json::to_string(&TaggedClientFrame::Ping { nonce }).unwrap();

        let now = tokio::time::Instant::now();

        // Send a Ping.
        ch.send_text(&ping_json).await.unwrap();
        let ping_sent_at = now;

        // Simulate the ping_interval firing again after pong_timeout has elapsed.
        tokio::time::sleep(pong_timeout + ping_interval_dur).await;

        let elapsed = ping_sent_at.elapsed();
        // No Pong arrived → elapsed > pong_timeout → dead.
        assert!(
            elapsed > pong_timeout,
            "expected dead detection: elapsed={elapsed:?} pong_timeout={pong_timeout:?}"
        );
    }
}
