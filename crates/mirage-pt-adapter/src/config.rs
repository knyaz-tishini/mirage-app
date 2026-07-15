//! Adapter-local config for the arc3 Phase-0 spike (contract §6).
//!
//! Every value below is a **placeholder** — the real chain_id / monitor /
//! mesh_id / bridge_target / forwarder_bind_addr / exit_public_addr only
//! exist once the test chain is built and the bridge box is provisioned
//! (contract §10 checklist steps 7-9, LIVE — explicitly out of scope for
//! this code-only pass).
//!
//! PT-v1 gives Tor no mechanism to pass custom options into a transport's
//! launch line beyond `exec <path> [args]` (contract §6.4) — there is no
//! room in `TOR_PT_CLIENT_TRANSPORTS` for values like chain_id or
//! bridge_target. So this binary reads its OWN config from adapter-owned
//! env vars (never read by Tor itself), each with a placeholder default —
//! wiring the live setup is then just exporting the right env var before
//! Tor launches this binary, no recompile needed.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tracing::warn;

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Parse a `SocketAddr` env var, falling back to `default` (and logging a
/// warning) on missing or malformed input — never panics on a bad
/// operator-supplied value.
fn env_socket_addr(name: &str, default: SocketAddr) -> SocketAddr {
    match std::env::var(name) {
        Ok(raw) => match raw.parse::<SocketAddr>() {
            Ok(addr) => addr,
            Err(_) => {
                warn!(env = name, value = %raw, %default, "invalid SocketAddr in env var, using placeholder default");
                default
            }
        },
        Err(_) => default,
    }
}

/// RFC 5737 TEST-NET-3 — deliberately non-routable placeholder so an
/// unconfigured run fails loudly at bind/dial time instead of silently
/// landing on a real address (contract §5.3: the real value is either an
/// RFC-1918 private interface or a firewalled public IP, decided at LIVE
/// setup after checking the bridge box's actual interfaces).
const DEFAULT_FORWARDER_BIND_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)), 38001);
const DEFAULT_EXIT_PUBLIC_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 20)), 38010);

const FORWARDER_BIND_ADDR_ENV: &str = "MIRAGE_PT_ADAPTER_FORWARDER_BIND_ADDR";
const EXIT_PUBLIC_ADDR_ENV: &str = "MIRAGE_PT_ADAPTER_EXIT_PUBLIC_ADDR";

/// Client-side (Mac test box) dial parameters (contract §3).
#[derive(Debug, Clone)]
pub struct ClientModeConfig {
    /// FILLED AT LIVE SETUP (contract §6.1) — chain_id returned by
    /// `POST /chains/build?via=<bridge-box-monitor-id>`.
    pub chain_id: String,
    /// FILLED AT LIVE SETUP (contract §6.1) — MachineId of the monitor the
    /// test chain was built through.
    pub via_monitor: String,
    /// FILLED AT LIVE SETUP — the monitor's public HTTP(S) surface used
    /// for the one-time `ChainBinding`+`PeerTable` fetch (contract §3 step
    /// 1, e.g. `"https://lm-xx.zen-geldmaschine.net:19443"`). This is a
    /// plain reqwest HTTPS call, NOT operator-box's own wire-authenticated
    /// `relay_get` — pt-adapter carries no operator SenderProof identity.
    pub monitor_base_url: String,
    /// FILLED AT LIVE SETUP (contract §6) — mesh_id bytes for the dial
    /// handshake. Defaults to the fleet's staging mesh id.
    pub mesh_id: Vec<u8>,
    /// FILLED AT LIVE SETUP (contract §5.3) — the bridge box's forwarder
    /// address. This is the ONE fixed target frame sent into every dialled
    /// chain, never the SOCKS5-requested destination (contract §3 step 4).
    pub bridge_target: String,
    /// FILLED AT LIVE SETUP (optional) — bearer token for the monitor's
    /// NodeAdmin-role `/relay/chains` + `/relay/peers` routes, if the live
    /// monitor's HTTP surface still enforces role auth on them. `None`
    /// sends the fetch unauthenticated.
    pub monitor_bearer: Option<String>,
}

impl ClientModeConfig {
    pub fn from_env() -> Self {
        Self {
            chain_id: env_or("MIRAGE_PT_ADAPTER_CHAIN_ID", "PLACEHOLDER-chain-id"),
            via_monitor: env_or(
                "MIRAGE_PT_ADAPTER_VIA_MONITOR",
                "PLACEHOLDER-monitor-machine-id",
            ),
            monitor_base_url: env_or(
                "MIRAGE_PT_ADAPTER_MONITOR_BASE_URL",
                "https://PLACEHOLDER-monitor-host:19443",
            ),
            mesh_id: env_or("MIRAGE_PT_ADAPTER_MESH_ID", "v3-staging").into_bytes(),
            bridge_target: env_or(
                "MIRAGE_PT_ADAPTER_BRIDGE_TARGET",
                "203.0.113.10:38001",
            ),
            monitor_bearer: std::env::var("MIRAGE_PT_ADAPTER_MONITOR_BEARER").ok(),
        }
    }
}

/// Server/bridge-side forwarder parameters (contract §4/§5.3).
#[derive(Debug, Clone)]
pub struct ServerModeConfig {
    /// The real (loopback) Tor ORPort. Sourced from the PT-supplied
    /// `TOR_PT_ORPORT` at runtime, not from a spike placeholder — Tor
    /// itself is authoritative for this value.
    pub or_port: SocketAddr,
    /// FILLED AT LIVE SETUP (contract §5.3) — RFC-1918 private interface
    /// if the bridge box has one (primary), else a firewalled public
    /// IP:port (disposable fallback, teardown-required per §5.3/§7.4).
    pub forwarder_bind_addr: SocketAddr,
    /// FILLED AT LIVE SETUP (contract §4/§6.3) — the mirage-node exit's
    /// real public listener address, reported verbatim in `SMETHOD`. Tor
    /// does not act on this value beyond syntactic validity — the
    /// mismatch vs. `TOR_PT_SERVER_BINDADDR` is intentional (§6.3 note).
    pub exit_public_addr: SocketAddr,
}

impl ServerModeConfig {
    /// Build server config from the live PT-supplied `or_port` (parsed in
    /// `pt_ipc::parse_env` from `TOR_PT_ORPORT`) plus this spike's own
    /// placeholder env vars for the two values PT-v1 has no room to carry.
    pub fn from_or_port(or_port: SocketAddr) -> Self {
        Self {
            or_port,
            forwarder_bind_addr: env_socket_addr(
                FORWARDER_BIND_ADDR_ENV,
                DEFAULT_FORWARDER_BIND_ADDR,
            ),
            exit_public_addr: env_socket_addr(EXIT_PUBLIC_ADDR_ENV, DEFAULT_EXIT_PUBLIC_ADDR),
        }
    }
}
