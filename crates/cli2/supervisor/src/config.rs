//! Supervisor configuration loaded from a TOML file.

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Top-level supervisor config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Base URL of mirage2monitor for OTA and health checks.
    pub monitor_url: String,
    /// Minisign public key (base64) baked into the binary for OTA verification.
    pub ota_pubkey: String,
    /// Seconds between OTA manifest polls.
    #[serde(default = "default_ota_poll")]
    pub ota_poll_interval_secs: u64,
    /// Path to the worker binary (relative or absolute).
    pub worker_binary: String,
    /// Base URL of the local TUN process for chain registry pushes.
    /// If unset or unreachable, supervisor logs a warning and continues.
    #[serde(default)]
    pub tun_api_url: Option<String>,
    /// Bearer token for `Authorization` header when talking to the monitor.
    /// Required for `POST /relay/clients` and `GET /relay/clients/subscribe`.
    /// If absent, requests are sent without auth (works only on monitors with
    /// open registration).
    #[serde(default)]
    pub monitor_api_key: Option<String>,
    /// Persist directory for supervisor state: `sentry-id.txt` and
    /// `last-peer-table.json`.  Created on first run if absent.
    /// Defaults to the OS temp dir / `mirage-cli2-state/`.
    #[serde(default)]
    pub state_dir: Option<String>,
    /// Chain definitions.
    pub chains: Vec<ChainConfig>,
    /// Optional router-level SOCKS5 listener and policy.
    ///
    /// When present, the supervisor binds an additional SOCKS5 listener at
    /// `routing.socks5_bind` and uses the configured policy to select which
    /// worker's SOCKS5 port to forward each connection to.
    #[serde(default)]
    pub routing: Option<RoutingConfig>,
}

/// Router policy used by the supervisor's own SOCKS5 listener.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoutingConfig {
    /// `"round_robin"` | `"latency_weighted"` | `"sticky"`. Default: round-robin.
    #[serde(default = "default_policy")]
    pub policy: String,
    /// Bind address for the supervisor-level SOCKS5 router listener.
    #[serde(default = "default_router_bind")]
    pub socks5_bind: String,
}

fn default_policy() -> String {
    "round_robin".to_string()
}

fn default_router_bind() -> String {
    "127.0.0.1:19489".to_string()
}

fn default_ota_poll() -> u64 {
    300
}

/// Single chain definition.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChainConfig {
    /// Unique chain identifier, e.g. `"byte-cipher-classic"`.
    pub id: String,
    /// Address + port the worker should bind its SOCKS5 listener on.
    pub socks5_bind: String,
    /// Path to the per-chain client config file passed to the worker.
    pub config_path: String,
}

impl Config {
    /// Load and parse a TOML config file.
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading config {path}"))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {path}"))?;
        Ok(config)
    }
}
