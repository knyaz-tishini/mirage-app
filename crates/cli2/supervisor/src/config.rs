//! Supervisor configuration loaded from a TOML file.

use std::net::SocketAddr;

use anyhow::Context;
use mirage_core::config::{
    ClientConfig, DnsConfig, HttpProxyConfig, MonitorTransportConfig as CoreMonitorTransport,
};
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

    /// Path to the config file that the supervisor re-reads for runtime chain
    /// adoption.  Defaults to the path passed on the CLI (populated by
    /// `main.rs` after load).  Setting this explicitly lets the sentinel
    /// materialise a *separate* chains config file while the supervisor config
    /// stays static.
    ///
    /// The supervisor polls `mtime` every `reload_poll_secs` seconds.  When the
    /// file changes, it reconciles the new `[[chains]]` set against running
    /// workers (spawn new, drain+kill removed, drain+kill+respawn changed).
    #[serde(default)]
    pub reload_config_path: Option<String>,

    /// Poll interval for config-file mtime checks (seconds).  Default: 5.
    #[serde(default = "default_reload_poll")]
    pub reload_poll_secs: u64,

    /// Optional bind address for the supervisor's local control HTTP listener.
    ///
    /// When set (e.g. `"127.0.0.1:19491"`), the supervisor binds a minimal
    /// HTTP server that exposes `POST /chains/reload`.  Posting to that
    /// endpoint triggers an immediate config reconcile (same code path as the
    /// poll loop) without waiting for the next mtime tick.
    ///
    /// Default: absent (no HTTP listener).
    #[serde(default)]
    pub control_http_bind: Option<String>,

    /// Optional local DoH DNS forwarder. When `dns.enabled = true` AND
    /// `[routing]` is configured, the supervisor binds a UDP DNS listener
    /// (`dns.listen_addr`) and tunnels queries (DoH) through the router's
    /// SOCKS5 egress, so DNS exits via the chains instead of leaking to the
    /// system resolver. `dns.socks_addr` is ignored — the upstream is pinned
    /// to the router's `socks5_bind`.
    #[serde(default)]
    pub dns: Option<DnsConfig>,

    /// Optional HTTP CONNECT proxy. When `http_proxy.enabled = true` AND
    /// `[routing]` is configured, the supervisor binds an HTTP CONNECT proxy at
    /// `http_proxy.bind_addr` and chains each CONNECT through the router's
    /// SOCKS5 egress. Process-wide singleton → supervisor-level, not per-worker.
    #[serde(default)]
    pub http_proxy: Option<HttpProxyConfig>,
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

fn default_reload_poll() -> u64 {
    5
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
    /// Load a config file, auto-detecting the format.
    ///
    /// A `.json` path (or a file whose first non-whitespace byte is `{`) is
    /// parsed as a [`ClientConfig`] — the unified config the sentinel
    /// materialises and the cli2 worker already reads (`worker/main.rs`) — and
    /// lowered into a supervisor `Config` via [`Config::from_client_config`].
    /// Any other path falls back to the legacy supervisor TOML.
    ///
    /// This is the production sentinel→cli2 path: ONE JSON, emitted by the
    /// sentinel, consumed by BOTH the supervisor (here) and every worker.
    pub fn load_any(path: &str) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading config {path}"))?;
        let is_json = path
            .rsplit('.')
            .next()
            .map(|e| e.eq_ignore_ascii_case("json"))
            .unwrap_or(false)
            || text.trim_start().starts_with('{');
        if is_json {
            let cc: ClientConfig = serde_json::from_str(&text)
                .with_context(|| format!("parsing client config JSON {path}"))?;
            Self::from_client_config(&cc, path)
        } else {
            toml::from_str::<Config>(&text).with_context(|| format!("parsing config {path}"))
        }
    }

    /// Lower a unified [`ClientConfig`] into a supervisor `Config`.
    ///
    /// The dynamic wire/chain spec (chains, mesh, monitor transports, creds via
    /// the worker) comes from the ClientConfig itself; the install-static
    /// orchestration knobs (`worker_binary`, `ota_pubkey`, router, …) come from
    /// its `supervisor` sub-object. `config_path` is the path the ClientConfig
    /// was read from — every worker is pointed back at it (each worker finds its
    /// own chain by `chain_id`), and it becomes the supervisor's mtime-watched
    /// reload source.
    pub fn from_client_config(cc: &ClientConfig, config_path: &str) -> anyhow::Result<Self> {
        let sup = cc.supervisor.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "client config {config_path} has no `supervisor` block — cannot run \
                 cli2-supervisor from it (cli1-only config?)"
            )
        })?;

        // MonitorLink-host designation: explicit chain_id, else the first chain.
        let ml_host: Option<String> = sup
            .monitor_link_host_chain
            .clone()
            .or_else(|| cc.chains.first().map(|c| c.chain_id.clone()));

        // Per-chain run-set: every worker reads THIS json (config_path) and finds
        // its own chain by id. A chain without a socks5_bind cannot host a worker
        // SOCKS5 ingress → hard error (caught here, not a silent dead worker).
        let mut chains = Vec::with_capacity(cc.chains.len());
        for c in &cc.chains {
            let socks5_bind = c.socks5_bind.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "chain '{}' has no socks5_bind — the cli2 worker needs a SOCKS5 \
                     ingress per chain; set chains[].socks5_bind in the client config",
                    c.chain_id
                )
            })?;
            chains.push(ChainConfig {
                id: c.chain_id.clone(),
                socks5_bind,
                config_path: config_path.to_string(),
                monitor_link_host: ml_host.as_deref() == Some(c.chain_id.as_str()),
            });
        }

        // Map monitor transports: mirage-core variants → supervisor variants.
        // The `Wire` transport is synthesised from the flat wire fields below.
        let mut monitor_transports: Vec<MonitorTransportConfig> = Vec::new();
        for t in &cc.monitor_transports {
            match t {
                CoreMonitorTransport::DirectWs { url, sni, .. } => {
                    monitor_transports.push(MonitorTransportConfig::DirectWs {
                        url: url.clone(),
                        sni: sni.clone(),
                    });
                }
                CoreMonitorTransport::OverChain {
                    chain_id,
                    monitor_endpoint,
                    ..
                } => match monitor_endpoint.parse::<SocketAddr>() {
                    Ok(addr) => monitor_transports.push(MonitorTransportConfig::OverChain {
                        chain_id: chain_id.clone(),
                        monitor_addr: addr,
                    }),
                    Err(e) => tracing::warn!(
                        "from_client_config: over-chain monitor_endpoint '{monitor_endpoint}' \
                         for chain {chain_id} is not a socket addr: {e} — transport skipped"
                    ),
                },
            }
        }
        // Synthesise a Wire transport carrying the real mesh_id so build_ml_setup's
        // flat-field branch resolves mesh_id from it (instead of defaulting).
        if let (Some(addr), Some(pubkey)) =
            (cc.monitor_wire_addr.clone(), cc.monitor_wire_pubkey_hex.clone())
        {
            monitor_transports.push(MonitorTransportConfig::Wire {
                monitor_addr: addr,
                monitor_pubkey_hex: pubkey,
                mesh_id: cc.mesh_id.clone(),
            });
        }

        let routing = sup.routing.as_ref().map(|r| RoutingConfig {
            policy: r.policy.clone(),
            socks5_bind: r.socks5_bind.clone(),
            chain_ids: r.chain_ids.clone(),
        });

        if cc.monitor_url.is_none() {
            tracing::warn!(
                "from_client_config: client config {config_path} has no monitor_url — \
                 HTTP peer_table bootstrap will be skipped (Wire/over-chain transport only)"
            );
        }

        Ok(Config {
            monitor_url: cc.monitor_url.clone().unwrap_or_default(),
            ota_pubkey: sup.ota_pubkey.clone(),
            ota_poll_interval_secs: sup.ota_poll_interval_secs,
            worker_binary: sup.worker_binary.clone(),
            tun_api_url: cc.tun_api_url.clone(),
            monitor_api_key: sup.monitor_api_key.clone(),
            state_dir: sup.state_dir.clone(),
            chains,
            monitor_transports,
            monitor_wire_addr: cc.monitor_wire_addr.clone(),
            monitor_wire_pubkey_hex: cc.monitor_wire_pubkey_hex.clone(),
            routing,
            reload_config_path: Some(config_path.to_string()),
            reload_poll_secs: sup.reload_poll_secs,
            control_http_bind: sup.control_http_bind.clone(),
            dns: Some(cc.dns.clone()),
            http_proxy: Some(cc.http_proxy.clone()),
        })
    }
}
