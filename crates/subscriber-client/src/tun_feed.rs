//! Optional feed into `mirage-client-core`'s `TunBridge` — points the TUN
//! virtual interface at this crate's own local, router-backed SOCKS5
//! listener.
//!
//! **Chosen default for an explicitly unresolved product question.**
//! `thin-customer-connector-2026-09-17.md` §5 and §8 #5 leave "TUN vs the
//! N-way sticky router" undecided: TUN's own chain registry
//! (`client-core/src/tun_bridge.rs::chain_to_json`) wants a per-chain
//! `socks5_addr` — a LOCAL listener address — so wiring one `MonitorChain`
//! per entrance would mean standing up a dedicated SOCKS5 listener per face
//! in addition to the shared, sharded one, which the plan explicitly does
//! not decide. This module instead feeds TUN a single named chain
//! (`TUN_FEED_CHAIN_ID`) whose `socks5_addr` points at THIS crate's own
//! shared listener — TUN gets one chain that already shards across every
//! entrance internally via the router, rather than zero sharding or a second
//! listener set per face. Easy to replace with a one-chain-per-entrance feed
//! once that product question is settled; nothing here forecloses it.
//!
//! The post is the same `/chains/update` body the fleet monitor bridge
//! sends for one healthy chain with no kill-switch. This crate does not
//! link the monitor channel.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::watch;
use tracing::warn;

/// Chain name reported to TUN for the shared, router-backed local listener.
pub const TUN_FEED_CHAIN_ID: &str = "subscriber-entrances";

/// Owns the background task that pushes the chain registry to TUN.
/// Dropping this stops further pushes. It does not unregister a chain TUN
/// already has.
pub struct TunFeed {
    _tx: watch::Sender<bool>,
    _watcher: tokio::task::JoinHandle<()>,
}

impl TunFeed {
    /// Start feeding `tun_api_url` with one named chain pointing at
    /// `socks5_bind` — see the module doc for why one shared chain, not one
    /// per entrance.
    pub fn start(tun_api_url: String, socks5_bind: SocketAddr) -> Self {
        let (tx, mut rx) = watch::channel(false);
        let socks5_addr = socks5_bind.to_string();
        let watcher = tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());
            let url = format!("{}/chains/update", tun_api_url.trim_end_matches('/'));
            let body = serde_json::json!({
                "chains": [{
                    "name": TUN_FEED_CHAIN_ID,
                    "socks5_addr": socks5_addr,
                    "protocol": "Tcp(Mirage)",
                    "failure_policy": { "mode": "fail_closed" },
                }]
            });
            loop {
                if rx.changed().await.is_err() {
                    break;
                }
                if !*rx.borrow() {
                    continue;
                }
                match http.post(&url).json(&body).send().await {
                    Ok(resp) if resp.status().is_success() => {}
                    Ok(resp) => warn!(status = %resp.status(), "TUN chains update rejected"),
                    Err(e) => warn!(error = %e, "TUN API unreachable, chains not updated"),
                }
            }
        });
        let _ = tx.send(true);
        Self { _tx: tx, _watcher: watcher }
    }
}
