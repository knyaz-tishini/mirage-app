//! Background link to mirage2monitor — config polling + heartbeat.
//!
//! [`MonitorLink`] registers with the monitor, periodically fetches the
//! latest chain config (using `If-None-Match` for cheap 304 responses), and
//! pushes updates through a [`tokio::sync::watch`] channel so the runtime can
//! react without polling.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};
use tracing::{debug, warn};

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
///
/// Carries the full `ChainConfig` materialized by the monitor (so the
/// client can spin up a TunnelPool / MuxPool from it directly) plus
/// monitor-only metadata (`healthy`, `latency_ms`) used by the runtime
/// for selection / display.
#[derive(Debug, Clone)]
pub struct MonitorChain {
    /// Full chain configuration materialized from monitor's stend registry.
    pub chain: mirage_core::config::ChainConfig,
    /// Live health flag from monitor.
    pub healthy: bool,
    /// Last measured latency to L1 (ms), if known.
    pub latency_ms: Option<u64>,
}

impl MonitorChain {
    /// Convenience accessor — chain id (mirrors `chain.chain_id`).
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

/// Wire-format mirror of monitor's `ClientConfigChain`: full inlined
/// `ChainConfig` plus monitor-only metadata. We just deserialize directly
/// into `mirage_core::config::ChainConfig` (with `#[serde(flatten)]`) and
/// pull the metadata fields off the side.
#[derive(Debug, Deserialize)]
struct ChainEntry {
    #[serde(flatten)]
    chain: mirage_core::config::ChainConfig,
    #[serde(default = "chain_entry_default_healthy")]
    healthy: bool,
    #[serde(default)]
    latency_ms: Option<u64>,
}

fn chain_entry_default_healthy() -> bool { true }

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

// ── MonitorLink ───────────────────────────────────────────────────────────────

/// Background task that keeps the client in sync with mirage2monitor.
pub struct MonitorLink {
    monitor_url: String,
    client_id: Mutex<Option<String>>,
    http: reqwest::Client,
    config_version: AtomicU64,
    poll_interval_secs: AtomicU64,
    config_tx: watch::Sender<Option<MonitorConfig>>,
}

impl MonitorLink {
    /// Create a new [`MonitorLink`] and return the watch receiver.
    ///
    /// The link does not start polling until [`MonitorLink::spawn`] is called.
    pub fn new(monitor_url: String) -> (Arc<Self>, watch::Receiver<Option<MonitorConfig>>) {
        let (config_tx, config_rx) = watch::channel(None);

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest::Client construction is infallible with these settings");

        let link = Arc::new(Self {
            monitor_url,
            client_id: Mutex::new(None),
            http,
            config_version: AtomicU64::new(0),
            poll_interval_secs: AtomicU64::new(30),
            config_tx,
        });

        (link, config_rx)
    }

    /// Create an additional [`watch::Receiver`] subscribed to the same config
    /// channel.  Useful when multiple consumers need independent cursors.
    pub fn subscribe(&self) -> watch::Receiver<Option<MonitorConfig>> {
        self.config_tx.subscribe()
    }

    /// Spawn the background polling loop.  Returns the task handle.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let link = Arc::clone(self);
        tokio::spawn(async move {
            link.run().await;
        })
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    async fn run(&self) {
        self.register().await;

        let interval_secs = self.poll_interval_secs.load(Ordering::Relaxed);
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        // First tick fires immediately; skip it so we don't double-poll right
        // after registration (register() already fetches initial config).
        interval.tick().await;

        loop {
            interval.tick().await;
            self.poll_config().await;
            self.send_heartbeat().await;
        }
    }

    async fn register(&self) {
        let url = format!("{}/relay/clients", self.monitor_url);
        let body = RegisterBody {
            os: std::env::consts::OS,
            version: env!("CARGO_PKG_VERSION"),
            supported_transports: vec!["tcp", "websocket"],
        };

        match self.http.post(&url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                match resp.json::<RegisterResponse>().await {
                    Ok(reg) => {
                        debug!(client_id = %reg.client_id, "registered with monitor");
                        self.poll_interval_secs
                            .store(reg.monitor_poll_interval_secs, Ordering::Relaxed);
                        *self.client_id.lock().await = Some(reg.client_id);
                        // Fetch initial config immediately after registration.
                        self.poll_config().await;
                    }
                    Err(e) => {
                        warn!(status = %status, "monitor register: failed to parse response: {e}");
                    }
                }
            }
            Err(e) => {
                warn!("monitor register: request failed: {e}");
            }
        }
    }

    async fn poll_config(&self) {
        let url = format!("{}/relay/clients/config", self.monitor_url);
        let current_version = self.config_version.load(Ordering::Relaxed);

        let req = self
            .http
            .get(&url)
            .header("If-None-Match", current_version.to_string());

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status == reqwest::StatusCode::NOT_MODIFIED {
                    debug!(version = current_version, "monitor config unchanged (304)");
                    return;
                }
                if !status.is_success() {
                    warn!(status = %status, "monitor config poll: unexpected status");
                    return;
                }
                match resp.json::<ConfigResponse>().await {
                    Ok(cfg) => {
                        debug!(version = cfg.version, chains = cfg.chains.len(), "monitor config updated");
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
                        // Ignore send error — if all receivers dropped, we're shutting down.
                        let _ = self.config_tx.send(Some(monitor_cfg));
                    }
                    Err(e) => {
                        warn!("monitor config poll: failed to parse response: {e}");
                    }
                }
            }
            Err(e) => {
                warn!("monitor config poll: request failed: {e}");
            }
        }
    }

    async fn send_heartbeat(&self) {
        let client_id = {
            let guard = self.client_id.lock().await;
            match guard.as_deref() {
                Some(id) => id.to_owned(),
                None => return, // not registered yet
            }
        };

        let url = format!("{}/relay/clients/{}/heartbeat", self.monitor_url, client_id);
        let active_chains = self.config_version.load(Ordering::Relaxed).min(u32::MAX as u64) as u32;
        let body = HeartbeatBody { active_chains };

        if let Err(e) = self.http.post(&url).json(&body).send().await {
            warn!("monitor heartbeat: request failed: {e}");
        }
    }
}
