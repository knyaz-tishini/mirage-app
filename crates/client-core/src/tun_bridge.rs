//! Bridge that forwards [`MonitorConfig`] updates to the TUN chain registry API.
//!
//! When a new config arrives on the watch channel, [`TunBridge`] posts the
//! healthy chains to `POST /chains/update` on the local TUN API.  All errors
//! are logged and swallowed — the TUN may not be running and that is fine.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::monitor_link::{MonitorConfig, MonitorChain};

/// Forwards monitor config updates to the TUN chain registry HTTP API.
pub struct TunBridge {
    tun_api_url: String,
    http: Client,
}

impl TunBridge {
    /// Create a new bridge pointing at `tun_api_url` (e.g. `"http://127.0.0.1:19500"`).
    pub fn new(tun_api_url: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self { tun_api_url, http }
    }

    /// Spawn a background task that watches `config_rx` and calls the TUN API
    /// whenever the config changes.  Returns the task handle.
    pub fn spawn_watcher(
        self: Arc<Self>,
        mut config_rx: watch::Receiver<Option<MonitorConfig>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while config_rx.changed().await.is_ok() {
                let config = config_rx.borrow().clone();
                if let Some(cfg) = config {
                    self.update_tun_chains(&cfg).await;
                }
            }
        })
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    async fn update_tun_chains(&self, config: &MonitorConfig) {
        let chains: Vec<serde_json::Value> = config
            .chains
            .iter()
            .filter(|c| c.healthy)
            .map(chain_to_json)
            .collect();

        let url = format!("{}/chains/update", self.tun_api_url);
        match self
            .http
            .post(&url)
            .json(&serde_json::json!({ "chains": chains }))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                info!(
                    version = config.version,
                    chains = chains.len(),
                    "TUN chains updated from monitor"
                );
            }
            Ok(resp) => {
                warn!(status = %resp.status(), "TUN chains update rejected");
            }
            Err(e) => {
                warn!(error = %e, "TUN API unreachable, chains not updated");
            }
        }
    }
}

fn chain_to_json(c: &MonitorChain) -> serde_json::Value {
    // TUN's /chains/update consumes a chain registry that maps a chain name
    // to a SOCKS5 listener address (the local port the client listens on
    // for that chain). When the chain has its own dedicated socks5_bind we
    // forward that. Without it we fall back to the L1 addr — historical
    // behavior preserved for compat. `protocol` is informational here.
    let socks5_addr: String = c
        .chain
        .socks5_bind
        .clone()
        .or_else(|| c.chain.hops.first().map(|h| h.addr.clone()))
        .unwrap_or_default();
    serde_json::json!({
        "name":        c.chain.chain_id,
        "socks5_addr": socks5_addr,
        "protocol":    format!("{:?}", c.chain.protocol),
    })
}
