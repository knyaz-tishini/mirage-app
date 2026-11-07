//! Push the chain → SOCKS5 port mapping to the local TUN process.
//!
//! Endpoint: POST {tun_api_url}/chains/update
//! Body: `{ "chains": [ { "name": "...", "socks5_addr": "..." }, ... ] }`
//!
//! If `tun_api_url` is not configured or the request fails, a warning is
//! logged and the supervisor continues normally.

use serde::Serialize;
use tracing::{info, warn};

use crate::config::Config;

#[derive(Debug, Serialize)]
struct ChainEntry<'a> {
    name: &'a str,
    socks5_addr: &'a str,
}

#[derive(Debug, Serialize)]
struct RegistryPayload<'a> {
    chains: Vec<ChainEntry<'a>>,
}

/// Push the current chain → SOCKS5 mapping to the TUN process.
///
/// Call after `wait_all_ready()` and again after every successful rolling update.
pub async fn push_tun_registry(config: &Config) {
    let url = match &config.tun_api_url {
        Some(u) => format!("{}/chains/update", u.trim_end_matches('/')),
        None => {
            // TUN integration not configured — skip silently.
            return;
        }
    };

    // Box-mode chains have no `socks5_bind` (a unix socket, not a registry
    // string the TUN process — a desktop-only component with no box-mode
    // counterpart — could dial anyway) and are skipped rather than sent as
    // an empty address: `tun_api_url` is `None` on every box-mode config in
    // practice, so this loop never runs at all there, but skipping is the
    // honest behaviour if it ever did.
    let chains: Vec<ChainEntry<'_>> = config
        .chains
        .iter()
        .filter_map(|c| {
            c.socks5_bind.as_deref().map(|socks5_addr| ChainEntry {
                name: &c.id,
                socks5_addr,
            })
        })
        .collect();

    let payload = RegistryPayload { chains };

    info!(url = %url, count = payload.chains.len(), "pushing chain registry to TUN");

    let client = reqwest::Client::new();
    match client.post(&url).json(&payload).send().await {
        Ok(resp) if resp.status().is_success() => {
            info!("TUN registry push succeeded");
        }
        Ok(resp) => {
            warn!(
                status = %resp.status(),
                "TUN registry push returned non-success status"
            );
        }
        Err(e) => {
            warn!("TUN registry push failed (TUN may not be running): {e}");
        }
    }
}
