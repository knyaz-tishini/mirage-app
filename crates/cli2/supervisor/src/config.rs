//! Supervisor configuration loaded from a TOML file.

use std::net::SocketAddr;

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
    /// Persist directory for supervisor state: `sentinel-id.txt` and
    /// `last-peer-table.json`.  Created on first run if absent.
    /// Defaults to the OS temp dir / `mirage-cli2-state/`.
    #[serde(default)]
    pub state_dir: Option<String>,
    /// Chain definitions.
    pub chains: Vec<ChainConfig>,
    /// Optional monitor transports list for [`MonitorLink`] fallback.
    ///
    /// When non-empty, `MonitorLink` receives these transports and will
    /// fall over to any configured `OverChain` transport when all `DirectWs`
    /// URLs fail. Default: empty (DirectWs-only, current behaviour).
    #[serde(default)]
    pub monitor_transports: Vec<MonitorTransportConfig>,
    /// IP:port of the monitor's Mirage wire listener, e.g. `"103.231.75.144:19444"`.
    ///
    /// When both `monitor_wire_addr` and `monitor_wire_pubkey_hex` are set, a
    /// `Wire` transport spec is prepended to the list sent to the MonitorLink
    /// host worker. The Mirage wire handshake provides mutual authentication;
    /// no bearer token is required.
    #[serde(default)]
    pub monitor_wire_addr: Option<String>,
    /// 64-character hex of the monitor's static X25519 public key.
    ///
    /// Required when `monitor_wire_addr` is set.
    #[serde(default)]
    pub monitor_wire_pubkey_hex: Option<String>,
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
    /// Optional whitelist of chain IDs that the router considers.
    /// When set, only listed chains rotate through the router. When unset,
    /// every chain in `[[chains]]` is eligible.
    #[serde(default)]
    pub chain_ids: Option<Vec<String>>,
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
    /// When `true`, the supervisor designates this worker as the MonitorLink host.
    ///
    /// The supervisor sends `SupervisorMsg::SetupMonitorLink` to exactly this worker
    /// after all workers are Ready. The worker constructs `MonitorLink` using its own
    /// `ChainSet` (single-entry, the chain it manages) so the OverChain transport can
    /// actually dial. Peer table snapshots flow back to supervisor via
    /// `WorkerMsg::PeerTableSnapshot` and are broadcast to all other workers.
    ///
    /// At most one chain should have `monitor_link_host = true`. If none do, the
    /// supervisor logs a WARN and runs without MonitorLink (legacy mode).
    #[serde(default)]
    pub monitor_link_host: bool,
}

/// TOML-serialisable mirror of `client_core::monitor_link::MonitorTransport`.
///
/// Kept separate so `supervisor/src/config.rs` does not need a direct dep on
/// `client-core` types at the config-parse level. `main.rs` converts these
/// to `MonitorTransport` before passing them to `MonitorLink::new`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum MonitorTransportConfig {
    /// Standard WebSocket subscribe.
    DirectWs {
        url: String,
        #[serde(default)]
        sni: Option<String>,
    },
    /// Binary frame protocol over a Mirage tunnel exit hop.
    OverChain {
        /// Chain ID to route through (must match a `[[chains]]` entry).
        chain_id: String,
        /// Loopback address of the monitor's over-chain listener.
        /// Defaults to `127.0.0.1:19445`.
        #[serde(default = "default_monitor_addr")]
        monitor_addr: SocketAddr,
    },
    /// Direct Mirage-wire WebSocket — dials the monitor's wire endpoint
    /// (`:19444`) using an ephemeral X25519 client key. Preferred over
    /// `DirectWs`; attempted first by `MonitorLink`.
    Wire {
        /// IP:port of the monitor's wire listener, e.g. `"103.231.75.144:19444"`.
        monitor_addr: String,
        /// 64-character hex of the monitor's static X25519 public key.
        monitor_pubkey_hex: String,
        /// Cluster/mesh identifier baked into the V9 handshake confirm.
        mesh_id: String,
    },
}

fn default_monitor_addr() -> SocketAddr {
    "127.0.0.1:19445".parse().expect("static addr is valid")
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
