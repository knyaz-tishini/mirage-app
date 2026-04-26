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
    /// Chain definitions.
    pub chains: Vec<ChainConfig>,
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
