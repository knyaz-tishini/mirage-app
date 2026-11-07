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
    /// IP:port of the monitor's Mirage wire listener, e.g. `"203.0.113.13:19444"`.
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

    /// Box mode: bind the router's client-facing listener on a box-local
    /// channel (`mirage_core::local_endpoint::LocalChannel`, a unix socket
    /// on the fleet, loopback TCP only on the Windows/test arm) instead of
    /// `socks5_bind`.
    ///
    /// The value names the channel by its stable label
    /// (`LocalChannel::label()`) — today only `"edge-client-entrance"`
    /// resolves (see `router_listener::resolve_local_channel`). Any other
    /// value is a hard config error at startup: the router listener refuses
    /// to start rather than silently falling back to the TCP `socks5_bind`
    /// port, because fleet boxes have no internal TCP ports (owner ruling
    /// 2026-09-23).
    ///
    /// `None` (the default): workstation mode, unchanged — TCP SOCKS5 on
    /// `socks5_bind`.
    ///
    /// This single field is the sole selector of "box mode" for the whole
    /// supervisor process — see [`Config::box_mode`]. It also governs how a
    /// worker is stopped: box mode never calls `child.kill()` (the
    /// supervisor's cgroup is kill-protected by the warden once armed, and
    /// every worker lives in the same cgroup), only the worker's own
    /// graceful IPC `Drain`/`Shutdown` path — see `worker_handle::StopOutcome`.
    #[serde(default)]
    pub listen_local_channel: Option<String>,
}

fn default_policy() -> String {
    "round_robin".to_string()
}

fn default_router_bind() -> String {
    "127.0.0.1:19489".to_string()
}

/// Box-mode per-worker SOCKS5 unix socket path.
///
/// The supervisor's router dials this (see `router_listener::WorkerSocksAddr`)
/// and tells the worker to bind it via `Socks5ListenSpec::Unix` at `Start`
/// time (`worker_handle::WorkerHandle::send_start`) — both sides derive the
/// SAME path from this one function so they can never disagree.
///
/// `workers/` is a PRIVATE subdirectory of the same runtime root
/// `mirage_core::local_endpoint::LocalChannel::EdgeClientEntrance` binds
/// into — derived from that channel's own declared path (never a second
/// hardcoded constant) so the two can never drift apart. Unlike the
/// entrance socket's directory (which grants the node account group
/// access), this one grants nothing: it is supervisor↔worker only, both
/// already the same uid, so the worker's own bind ceremony
/// (`mirage_client_core::socks5::bind_unix_socks5_listener`) sets it 0700
/// owner-only every time it binds.
pub fn worker_socks5_unix_path(chain_id: &str) -> std::path::PathBuf {
    let entrance = mirage_core::local_endpoint::LocalChannel::EdgeClientEntrance.unix_path();
    let base = std::path::Path::new(entrance)
        .parent()
        .map(|p| p.join("workers"))
        .unwrap_or_else(|| std::path::PathBuf::from("/run/mirage/edge-client/workers"));
    base.join(format!("worker-{chain_id}.sock"))
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
    ///
    /// Required in workstation mode, MUST be absent in box mode — box mode
    /// never binds a TCP port (owner ruling 2026-09-23: fleet boxes have no
    /// internal TCP ports). The worker binds a unix socket instead, at a
    /// path the supervisor derives itself (`worker_socks5_unix_path`) and
    /// sends over `Start` as `Socks5ListenSpec::Unix`. See [`Config::validate`],
    /// which refuses a config that gets this backwards in either direction.
    #[serde(default)]
    pub socks5_bind: Option<String>,
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
    /// Mirrors `mirage_core::config::ChainConfig::kill_switch`. Forwarded at
    /// `from_client_config` lowering time so the router-level gate applies
    /// the same FailClosed/Failover{group} contract as the worker gate.
    #[serde(default)]
    pub kill_switch: Option<mirage_core::config::KillSwitchMode>,
    #[serde(default)]
    pub group: Option<String>,
    /// Mirrors `ChainConfig::effective_nature()` (already resolved by the
    /// time it lands here — see `from_client_config` below).
    #[serde(default)]
    pub nature: mirage_core::config::WireNature,
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
        /// IP:port of the monitor's wire listener, e.g. `"203.0.113.13:19444"`.
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
    /// Whether this supervisor process runs in box mode — the `edge-client`
    /// Mirage unit under the warden, entrance on a box-local channel, workers
    /// stopped only over their own graceful IPC path, never `kill()`.
    ///
    /// Sole selector: `routing.listen_local_channel` being set. There is
    /// deliberately no second flag — a box that binds the box-local entrance
    /// but still tries `kill()` on a stuck worker (denied EPERM once the
    /// warden arms the cgroup) is a worse state than either mode cleanly
    /// applied, so one field decides both the listener transport
    /// (`router_listener::resolve_local_channel`) and the worker-stop
    /// mechanism (`worker_handle::WorkerHandle::stop`).
    pub fn box_mode(&self) -> bool {
        self.routing
            .as_ref()
            .and_then(|r| r.listen_local_channel.as_deref())
            .is_some()
    }

    /// Validate the shape that spans every chain: `socks5_bind` must be
    /// present in workstation mode (the worker needs a real TCP bind
    /// target) and ABSENT in box mode — fail closed, exactly like the
    /// router's own `listen_local_channel` refusal in `main.rs`. Box mode
    /// never binds a TCP port (owner ruling 2026-09-23); a chain that still
    /// sets `socks5_bind` there is refused rather than silently ignored, so
    /// a config left over from a workstation deployment can never turn into
    /// a live TCP listener on a fleet box by accident.
    pub fn validate(&self) -> anyhow::Result<()> {
        let box_mode = self.box_mode();
        for chain in &self.chains {
            match (box_mode, &chain.socks5_bind) {
                (true, Some(_)) => anyhow::bail!(
                    "box mode: chain '{}' sets socks5_bind — box mode never binds a TCP port; \
                     remove it, the worker binds a unix socket automatically \
                     (see worker_socks5_unix_path)",
                    chain.id
                ),
                (false, None) => anyhow::bail!(
                    "chain '{}' has no socks5_bind — workstation mode requires a TCP SOCKS5 \
                     bind address per chain",
                    chain.id
                ),
                _ => {}
            }
        }
        Ok(())
    }

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

        mirage_core::config::ChainConfig::validate_group_natures(&cc.chains)
            .with_context(|| format!("client config {config_path} failed nature validation"))?;

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
                // Sentinel-managed JSON is always workstation mode (box mode
                // is operator-authored TOML only, see `Config::box_mode`) —
                // always `Some` here.
                socks5_bind: Some(socks5_bind),
                config_path: config_path.to_string(),
                monitor_link_host: ml_host.as_deref() == Some(c.chain_id.as_str()),
                kill_switch: c.kill_switch.clone(),
                group: c.group.clone(),
                nature: c.effective_nature(),
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
            // Box mode is never selected via the sentinel-managed JSON path —
            // `mirage_core::config`'s routing sub-object carries no such
            // field. Only an operator-authored `supervisor.toml` for the
            // dedicated `edge-client` unit sets it (parsed directly by
            // `load_any`'s TOML branch, below `from_client_config`).
            listen_local_channel: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    const BOX_MODE_TOML: &str = r#"
monitor_url = "https://lm-nl.example.net:19443"
ota_pubkey = "deadbeef"
worker_binary = "mirage-chain-worker"

[[chains]]
id = "edge-exit-1"
config_path = "/etc/mirage/edge-client-config.json"

[routing]
listen_local_channel = "edge-client-entrance"
"#;

    #[test]
    fn box_mode_toml_parses_listen_local_channel() {
        let cfg: Config = toml::from_str(BOX_MODE_TOML).expect("box-mode TOML parses");
        let routing = cfg.routing.as_ref().expect("[routing] present");
        assert_eq!(
            routing.listen_local_channel.as_deref(),
            Some("edge-client-entrance")
        );
        assert!(cfg.box_mode(), "box_mode() must be true once listen_local_channel is set");
        assert_eq!(cfg.chains[0].socks5_bind, None, "box-mode chain sets no socks5_bind");
        cfg.validate().expect("a clean box-mode config (no socks5_bind) validates");
    }

    #[test]
    fn validate_refuses_socks5_bind_in_box_mode() {
        let toml_text = r#"
monitor_url = "https://lm-nl.example.net:19443"
ota_pubkey = "deadbeef"
worker_binary = "mirage-chain-worker"

[[chains]]
id = "edge-exit-1"
socks5_bind = "127.0.0.1:19500"
config_path = "/etc/mirage/edge-client-config.json"

[routing]
listen_local_channel = "edge-client-entrance"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("TOML parses (validate catches it, not serde)");
        let err = cfg.validate().expect_err("box mode + socks5_bind must be refused");
        assert!(
            err.to_string().contains("socks5_bind"),
            "error should name the offending field: {err}"
        );
    }

    #[test]
    fn validate_refuses_missing_socks5_bind_in_workstation_mode() {
        let toml_text = r#"
monitor_url = "https://lm-nl.example.net:19443"
ota_pubkey = "deadbeef"
worker_binary = "mirage-chain-worker"

[[chains]]
id = "c1"
config_path = "/etc/mirage/client-config.json"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("TOML parses (validate catches it, not serde)");
        let err = cfg.validate().expect_err("workstation mode without socks5_bind must be refused");
        assert!(
            err.to_string().contains("socks5_bind"),
            "error should name the offending field: {err}"
        );
    }

    #[test]
    fn worker_socks5_unix_path_is_derived_from_the_entrance_channel_dir() {
        let path = worker_socks5_unix_path("edge-exit-1");
        // Component-wise comparison, not a string prefix: `PathBuf::join` uses
        // the native separator, so a literal "/a/b/" string prefix check
        // would spuriously fail on Windows (dev/test box) even though the
        // path is only ever real on the Linux fleet.
        let expected_dir = std::path::Path::new("/run/mirage/edge-client").join("workers");
        assert!(
            path.starts_with(&expected_dir),
            "expected a private subdirectory of the entrance channel's own runtime dir \
             ({}), got {}",
            expected_dir.display(),
            path.display()
        );
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("worker-edge-exit-1.sock")
        );
    }

    #[test]
    fn workstation_toml_has_no_box_mode() {
        let toml_text = r#"
monitor_url = "https://lm-nl.example.net:19443"
ota_pubkey = "deadbeef"
worker_binary = "mirage-chain-worker"

[[chains]]
id = "c1"
socks5_bind = "127.0.0.1:19500"
config_path = "/etc/mirage/client-config.json"

[routing]
socks5_bind = "127.0.0.1:19489"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("workstation TOML parses");
        let routing = cfg.routing.as_ref().expect("[routing] present");
        assert_eq!(routing.listen_local_channel, None);
        assert!(!cfg.box_mode(), "box_mode() must stay false without listen_local_channel");
    }

    #[test]
    fn no_routing_block_is_not_box_mode() {
        let toml_text = r#"
monitor_url = "https://lm-nl.example.net:19443"
ota_pubkey = "deadbeef"
worker_binary = "mirage-chain-worker"

[[chains]]
id = "c1"
socks5_bind = "127.0.0.1:19500"
config_path = "/etc/mirage/client-config.json"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("no-routing TOML parses");
        assert!(cfg.routing.is_none());
        assert!(!cfg.box_mode());
    }
}
