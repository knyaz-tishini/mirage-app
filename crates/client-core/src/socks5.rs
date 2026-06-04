//! Minimal SOCKS5 server (RFC 1928).
//!
//! Supports CONNECT (0x01) and UDP ASSOCIATE (0x03) with no authentication.
//! Handles both IPv4 (0x01), domain (0x03), and IPv6 (0x04) addresses.
//!
//! ## Multi-chain failover + kill-switch (Wave 2a)
//!
//! Each [`Socks5Server`] is bound to one chain's [`TunnelPool`] as its
//! primary pool.  An optional `failover_pools` list (sorted by priority)
//! allows the handler to try other chains when the primary pool is exhausted.
//!
//! Kill-switch invariants enforced here:
//!
//! 1. **Bypass is sacred** — `RouteDecision::Direct` flows are always allowed
//!    through `route_direct` (native TCP).  The kill-switch NEVER blocks a
//!    flow that the routing table designated as Direct/Bypass.
//!
//! 2. **No native internet for wire flows** — `RouteDecision::Chain` flows
//!    MUST NOT fall through to `route_direct` on pool exhaustion.  On failure
//!    they receive a SOCKS5 error reply and are closed.
//!
//! 3. **Per-wire class** —
//!    - `SocksFailurePolicy::FailClosed` (Strict): on primary pool failure → SOCKS5
//!      error, no reroute, no native.
//!    - `SocksFailurePolicy::Failover { group }` (General): try only pools whose
//!      chain belongs to the same group; if all fail → SOCKS5 error, no native.
//!    - `SocksFailurePolicy::Default` (no kill-switch configured): legacy behavior,
//!      try all failover pools in order.

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use mirage_core::{config::{ClientConfig, KillSwitchMode}, error::MirageError, routing::RouteDecision, routing::RoutingTable};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use mirage_protocol::traits::Tunnel as _;

use crate::{mux_pool::MuxPool, pool::TunnelPool, relay::{relay_loop, TunnelCompat}};
use crate::udp_framing::{write_udp_frame, read_udp_frame};
use crate::chain_set::{ChainId, Router, TargetKey, FlowKey, ProtocolHint};
use crate::wire_health::WireHealthMap;

// ── Kill-switch policy for the SOCKS5 path ───────────────────────────────────

/// Per-chain failure policy for the SOCKS5 routing path.
///
/// Maps from [`KillSwitchMode`] + `group` in [`ChainConfig`]:
/// - `None` → `Default` (legacy: try all failover pools unconditionally)
/// - `Strict` → `FailClosed`
/// - `General` + group → `Failover { group }`
///
/// [`KillSwitchMode`]: mirage_core::config::KillSwitchMode
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocksFailurePolicy {
    /// No kill-switch configured for this chain — legacy failover semantics.
    Default,
    /// INVARIANT 3 / STRICT: flow pinned to its exact wire.
    ///
    /// On primary pool failure → send SOCKS5 error, DO NOT try other pools,
    /// DO NOT call `route_direct`.
    FailClosed,
    /// INVARIANT 3 / GENERAL: flow may use any healthy wire in the same group.
    ///
    /// On primary failure → try only pools whose chain is in `group`.
    /// If all group members fail → SOCKS5 error, DO NOT call `route_direct`.
    Failover { group: String },
}

impl SocksFailurePolicy {
    /// Derive a `SocksFailurePolicy` from a chain's kill-switch config fields.
    pub fn from_chain_config(
        kill_switch: Option<&KillSwitchMode>,
        group: Option<&str>,
        chain_id: &str,
    ) -> Self {
        match kill_switch {
            None => SocksFailurePolicy::Default,
            Some(KillSwitchMode::Strict) => SocksFailurePolicy::FailClosed,
            Some(KillSwitchMode::General) => {
                // Use explicit group name, fall back to chain_id as group sentinel.
                let g = group.unwrap_or(chain_id).to_owned();
                SocksFailurePolicy::Failover { group: g }
            }
        }
    }
}

#[cfg(target_os = "linux")]
use crate::transparent::{get_original_dst, peek_connection_kind, ConnectionKind};

// SOCKS5 constants.
const SOCKS5_VERSION: u8 = 0x05;
const SOCKS5_AUTH_NONE: u8 = 0x00;
const SOCKS5_CMD_CONNECT: u8 = 0x01;
const SOCKS5_CMD_UDP_ASSOCIATE: u8 = 0x03;
const SOCKS5_ATYP_IPV4: u8 = 0x01;
const SOCKS5_ATYP_DOMAIN: u8 = 0x03;
const SOCKS5_ATYP_IPV6: u8 = 0x04;
const SOCKS5_REP_SUCCESS: u8 = 0x00;
/// RFC 1928 §6 reply code: general SOCKS server failure.
/// Used for kill-switch fail-closed: wire flow → no wire available → refuse.
const SOCKS5_REP_GENERAL_FAILURE: u8 = 0x01;
const SOCKS5_REP_CMD_NOT_SUPPORTED: u8 = 0x07;
const SOCKS5_REP_ATYP_NOT_SUPPORTED: u8 = 0x08;

/// The parsed destination from a SOCKS5 CONNECT request.
enum Destination {
    Ip(SocketAddr),
    Domain(String, u16),
}

impl Destination {
    /// Format as `"host:port"` string suitable for TCP connect.
    fn to_addr_string(&self) -> String {
        match self {
            Self::Ip(addr) => addr.to_string(),
            Self::Domain(host, port) => format!("{host}:{port}"),
        }
    }
}

/// A minimal SOCKS5 proxy server.
///
/// Each instance is bound to one chain's tunnel pool.  An optional
/// `failover_pools` list (ordered highest-priority first, **excluding** the
/// primary pool) enables cross-chain failover when the primary pool is
/// exhausted.
///
/// When `chain_router` is set, it overrides the static `failover_pools`
/// ordering: `Router::pick(target, flow)` returns an ordered `Vec<ChainId>`
/// which is used to look up pools in `all_pools`. If a chain_id is not found
/// in `all_pools`, it is silently skipped. When `chain_router` is `None`,
/// the legacy `failover_pools` path is used unchanged (cli1 backward compat).
///
/// ## Kill-switch
///
/// `chain_policies` maps each chain_id (including the primary) to its
/// [`SocksFailurePolicy`].  When populated, the failover logic in
/// `route_via_pool_with_failover` enforces per-wire Strict/General semantics.
/// Chains absent from the map default to `SocksFailurePolicy::Default`.
///
/// `wire_health` tracks per-wire liveness so dead wires fail closed
/// immediately instead of waiting for the pool-lease timeout (300–600 s).
pub struct Socks5Server {
    bind_addr: SocketAddr,
    /// `(chain_id, pool)` pairs used for failover, highest-priority first.
    /// Does not include the primary pool. Used when `chain_router` is `None`.
    failover_pools: Vec<(String, Arc<TunnelPool>)>,
    /// Dynamic chain router. When set, overrides `failover_pools` ordering.
    chain_router: Option<Arc<dyn Router>>,
    /// All pool handles keyed by chain_id — used when `chain_router` is set.
    all_pools: Arc<HashMap<ChainId, Arc<TunnelPool>>>,
    /// Per-chain kill-switch failure policies (chain_id → policy).
    /// Absent entry → `SocksFailurePolicy::Default` (legacy behavior).
    chain_policies: Arc<HashMap<String, SocksFailurePolicy>>,
    /// Per-wire liveness map.  Shared across all connection handlers spawned
    /// by this server so a dead wire is detected instantly without waiting for
    /// pool-lease timeouts.
    wire_health: Arc<WireHealthMap>,
    /// `chain_id` this server instance is the primary for.
    primary_chain_id: String,
}

impl Socks5Server {
    /// Create a new server that will listen on `bind_addr` with no failover.
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self {
            bind_addr,
            failover_pools: Vec::new(),
            chain_router: None,
            all_pools: Arc::new(HashMap::new()),
            chain_policies: Arc::new(HashMap::new()),
            wire_health: WireHealthMap::new(),
            primary_chain_id: String::new(),
        }
    }

    /// Create a server that will listen on `bind_addr` with cross-chain
    /// failover pools (sorted highest-priority first, primary excluded).
    pub fn new_with_failover(
        bind_addr: SocketAddr,
        failover_pools: Vec<(String, Arc<TunnelPool>)>,
    ) -> Self {
        Self {
            bind_addr,
            failover_pools,
            chain_router: None,
            all_pools: Arc::new(HashMap::new()),
            chain_policies: Arc::new(HashMap::new()),
            wire_health: WireHealthMap::new(),
            primary_chain_id: String::new(),
        }
    }

    /// Attach kill-switch policies to an existing server.
    ///
    /// `policies` maps every chain_id (including the primary) to its
    /// [`SocksFailurePolicy`].  Chains absent from the map retain
    /// `SocksFailurePolicy::Default`.  `primary_chain_id` identifies which
    /// chain is the primary for this server instance so the handler can look
    /// up its own policy without ambiguity.
    pub fn with_kill_switch(
        mut self,
        primary_chain_id: String,
        policies: HashMap<String, SocksFailurePolicy>,
    ) -> Self {
        self.primary_chain_id = primary_chain_id;
        self.chain_policies = Arc::new(policies);
        self
    }

    /// Attach a shared wire-health map (for fleets of servers sharing the same
    /// underlying wire set).  When not called, a fresh per-server map is used.
    pub fn with_shared_wire_health(mut self, wire_health: Arc<WireHealthMap>) -> Self {
        self.wire_health = wire_health;
        self
    }

    /// Create a server with a dynamic `Router` driving chain selection.
    ///
    /// `all_pools` must contain every chain pool the router may return,
    /// keyed by `ChainId`. When the router returns a chain not present in
    /// `all_pools`, that chain is skipped and the next candidate is tried.
    pub fn new_with_router(
        bind_addr: SocketAddr,
        chain_router: Arc<dyn Router>,
        all_pools: HashMap<ChainId, Arc<TunnelPool>>,
    ) -> Self {
        Self {
            bind_addr,
            failover_pools: Vec::new(),
            chain_router: Some(chain_router),
            all_pools: Arc::new(all_pools),
            chain_policies: Arc::new(HashMap::new()),
            wire_health: WireHealthMap::new(),
            primary_chain_id: String::new(),
        }
    }

    /// Accept connections forever, spawning a handler task per connection.
    pub async fn run(
        &self,
        pool: Arc<TunnelPool>,
        mux_pool: Option<Arc<MuxPool>>,
        _config: Arc<ClientConfig>,
        router: Arc<RoutingTable>,
    ) -> Result<(), MirageError> {
        let listener = TcpListener::bind(self.bind_addr).await?;
        info!(addr = %self.bind_addr, "SOCKS5 server listening");

        let failover = Arc::new(self.failover_pools.clone());
        let chain_router = self.chain_router.clone();
        let all_pools = Arc::clone(&self.all_pools);
        let chain_policies = Arc::clone(&self.chain_policies);
        let wire_health = Arc::clone(&self.wire_health);
        let primary_chain_id = Arc::new(self.primary_chain_id.clone());

        loop {
            let (stream, peer) = listener.accept().await?;
            debug!(%peer, "new SOCKS5 connection");

            let pool = Arc::clone(&pool);
            let mux_pool = mux_pool.clone();
            let router = Arc::clone(&router);
            let failover = Arc::clone(&failover);
            let chain_router = chain_router.clone();
            let all_pools = Arc::clone(&all_pools);
            let chain_policies = Arc::clone(&chain_policies);
            let wire_health = Arc::clone(&wire_health);
            let primary_chain_id = Arc::clone(&primary_chain_id);

            tokio::spawn(async move {
                #[cfg(target_os = "linux")]
                let result = {
                    let kind = match peek_connection_kind(&stream).await {
                        Ok(k) => k,
                        Err(e) => {
                            error!(%peer, "connection peek error: {e}");
                            return;
                        }
                    };
                    match kind {
                        ConnectionKind::Socks5 => {
                            handle_socks5_connection(stream, peer, pool, mux_pool, router, failover, chain_router, all_pools, chain_policies, wire_health, primary_chain_id).await
                        }
                        ConnectionKind::Transparent => {
                            handle_transparent_connection(stream, pool, mux_pool, router, failover, chain_router, all_pools, chain_policies, wire_health, primary_chain_id).await
                        }
                    }
                };

                #[cfg(not(target_os = "linux"))]
                let result = handle_socks5_connection(stream, peer, pool, mux_pool, router, failover, chain_router, all_pools, chain_policies, wire_health, primary_chain_id).await;

                if let Err(e) = result {
                    error!(%peer, "connection error: {e}");
                }
            });
        }
    }

    /// Run the SOCKS5 server with the simplest possible setup:
    /// every CONNECT goes through `pool` via the chain, no routing rules,
    /// no mux pool, no failover.
    ///
    /// Used by single-chain workers (mirage2split) where each worker process
    /// owns exactly one chain and one bind address.
    pub async fn run_simple(&self, pool: Arc<TunnelPool>) -> Result<(), MirageError> {
        let listener = TcpListener::bind(self.bind_addr).await?;
        info!(addr = %self.bind_addr, "SOCKS5 server (simple) listening");

        loop {
            let (stream, peer) = listener.accept().await?;
            debug!(%peer, "new SOCKS5 connection (simple)");
            let pool = Arc::clone(&pool);

            tokio::spawn(async move {
                if let Err(e) = handle_socks5_simple(stream, pool).await {
                    error!(%peer, "connection error: {e}");
                }
            });
        }
    }
}

/// Handle a single SOCKS5 connection — always route via the chain pool.
/// No routing decision, no failover, no mux.
async fn handle_socks5_simple(
    mut stream: TcpStream,
    pool: Arc<TunnelPool>,
) -> Result<(), MirageError> {
    // ── 1. SOCKS5 greeting ───────────────────────────────────────────────────
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await?;
    if header[0] != SOCKS5_VERSION {
        return Err(MirageError::Protocol(format!(
            "expected SOCKS5 version 5, got {}",
            header[0]
        )));
    }
    let n_methods = header[1] as usize;
    if n_methods == 0 {
        return Err(MirageError::Protocol("SOCKS5 greeting: zero methods".into()));
    }
    let mut methods = vec![0u8; n_methods];
    stream.read_exact(&mut methods).await?;
    stream.write_all(&[SOCKS5_VERSION, SOCKS5_AUTH_NONE]).await?;

    // ── 2. SOCKS5 request ────────────────────────────────────────────────────
    let mut req_header = [0u8; 4];
    stream.read_exact(&mut req_header).await?;
    if req_header[0] != SOCKS5_VERSION {
        return Err(MirageError::Protocol("SOCKS5 request: bad version".into()));
    }
    let cmd = req_header[1];
    let atyp = req_header[3];

    match cmd {
        SOCKS5_CMD_CONNECT => {
            let dest = parse_destination(&mut stream, atyp).await?;
            let target_addr = dest.to_addr_string();
            debug!(target = %target_addr, "routing decision: chain (simple)");

            let mut lease = lease_and_send_target(&pool, &target_addr, LEASE_SEND_MAX_ATTEMPTS).await?;
            send_socks5_success(&mut stream).await?;
            lease.set_relay_active(true);
            let tunnel = lease.take_tunnel().ok_or(MirageError::TunnelClosed)?;
            let mut compat = TunnelCompat::wrap(tunnel);
            let result = relay_loop(&mut stream, &mut compat).await;
            lease.poison();
            result
        }
        SOCKS5_CMD_UDP_ASSOCIATE => {
            send_socks5_error(&mut stream, SOCKS5_REP_CMD_NOT_SUPPORTED).await;
            Err(MirageError::Protocol(
                "SOCKS5 UDP ASSOCIATE not supported in simple mode".into(),
            ))
        }
        _ => {
            send_socks5_error(&mut stream, SOCKS5_REP_CMD_NOT_SUPPORTED).await;
            Err(MirageError::Protocol(format!(
                "SOCKS5 unsupported command: 0x{cmd:02x}"
            )))
        }
    }
}

/// How many times to retry the lease+send-target sequence before giving up.
/// Retries are safe (pre-SOCKS5-success, idempotent) and absorb cold-pool /
/// single-use-tunnel first-request flakiness.
const LEASE_SEND_MAX_ATTEMPTS: u32 = 3;

/// Lease a tunnel and send the target address, retrying the whole
/// lease+send sequence on failure. This is safe to retry because it runs
/// entirely BEFORE SOCKS5 success is sent to the client (no guest bytes
/// have flowed yet). Returns a lease with the target address already sent,
/// ready for the caller to send SOCKS5 success + enter the relay loop.
/// De-flakes cold-pool / single-use-tunnel first-request failures (#59).
async fn lease_and_send_target(
    pool: &Arc<TunnelPool>,
    target_addr: &str,
    max_attempts: u32,
) -> Result<crate::pool::TunnelLease, MirageError> {
    let mut last_err = MirageError::PoolExhausted;
    for attempt in 0..max_attempts {
        let mut lease = match pool.lease().await {
            Ok(l) => l,
            Err(e) => {
                last_err = e;
                // brief backoff to let the pool warm a slot, then retry
                tokio::time::sleep(std::time::Duration::from_millis(150 * (attempt as u64 + 1))).await;
                continue;
            }
        };
        let send_result = if lease.is_onion {
            let target_bytes = target_addr.as_bytes();
            let len_prefix = (target_bytes.len() as u16).to_be_bytes();
            let mut framed = Vec::with_capacity(2 + target_bytes.len());
            framed.extend_from_slice(&len_prefix);
            framed.extend_from_slice(target_bytes);
            lease.send(&framed).await
        } else {
            lease.send(target_addr.as_bytes()).await
        };
        match send_result {
            Ok(()) => return Ok(lease),
            Err(e) => {
                // dead/stale tunnel — poison it so the pool rebuilds, retry
                lease.poison();
                drop(lease);
                last_err = e;
                tokio::time::sleep(std::time::Duration::from_millis(150 * (attempt as u64 + 1))).await;
                continue;
            }
        }
    }
    warn!(target = %target_addr, attempts = max_attempts, error = %last_err, "lease_and_send_target: all attempts failed");
    Err(last_err)
}

/// Lease a tunnel from the primary pool, falling over to other chains when
/// the primary is exhausted.
///
/// Legacy path used by the UDP session task and other callers that predate
/// kill-switch.  Does NOT enforce `SocksFailurePolicy`.  For the transparent
/// proxy path with kill-switch, use [`lease_with_failover_policy`] instead.
async fn lease_with_failover(
    primary: &Arc<TunnelPool>,
    failover: &[(String, Arc<TunnelPool>)],
) -> Result<crate::pool::TunnelLease, MirageError> {
    match primary.lease().await {
        Ok(lease) => return Ok(lease),
        Err(e) => {
            warn!(error = %e, "primary pool lease failed, trying failover");
        }
    }

    for (chain_id, pool) in failover {
        warn!(chain = %chain_id, "failing over to chain");
        match pool.lease().await {
            Ok(lease) => {
                info!(chain = %chain_id, "failover: using chain");
                return Ok(lease);
            }
            Err(e) => {
                warn!(chain = %chain_id, error = %e, "failover chain also failed");
                continue;
            }
        }
    }

    Err(MirageError::PoolExhausted)
}

/// Policy-aware lease for the transparent proxy path.
///
/// Same kill-switch semantics as [`route_via_pool_with_failover`] but does not
/// send SOCKS5 error replies (the transparent path has no SOCKS5 framing).
/// On any failure returns `Err` — the caller RSTs the connection.
///
/// INVARIANT 2: never falls through to native TCP on exhaustion.
#[cfg(target_os = "linux")]
async fn lease_with_failover_policy(
    primary: &Arc<TunnelPool>,
    primary_chain_id: &str,
    failover: &[(String, Arc<TunnelPool>)],
    policy: &SocksFailurePolicy,
    wire_health: &Arc<WireHealthMap>,
    chain_policies: &HashMap<String, SocksFailurePolicy>,
) -> Result<crate::pool::TunnelLease, MirageError> {
    match primary.lease().await {
        Ok(lease) => {
            wire_health.record_success(primary_chain_id);
            return Ok(lease);
        }
        Err(primary_err) => {
            wire_health.record_failure(primary_chain_id);
            match policy {
                // INVARIANT 3 / STRICT: no failover.
                SocksFailurePolicy::FailClosed => {
                    warn!(
                        chain = %primary_chain_id,
                        error = %primary_err,
                        "transparent kill-switch FailClosed: refusing flow"
                    );
                    return Err(primary_err);
                }
                // INVARIANT 3 / GENERAL: group-only.
                SocksFailurePolicy::Failover { group } => {
                    warn!(
                        chain = %primary_chain_id,
                        error = %primary_err,
                        group = %group,
                        "transparent kill-switch Failover: trying group members"
                    );
                    let mut last_err = primary_err;
                    for (chain_id, fo_pool) in failover {
                        let in_group = match chain_policies.get(chain_id) {
                            Some(SocksFailurePolicy::Failover { group: g }) => g == group,
                            _ => false,
                        };
                        if !in_group {
                            continue;
                        }
                        if !wire_health.is_healthy(chain_id) {
                            warn!(chain = %chain_id, "wire-health: transparent group member unhealthy");
                            continue;
                        }
                        match fo_pool.lease().await {
                            Ok(l) => {
                                wire_health.record_success(chain_id);
                                return Ok(l);
                            }
                            Err(e) => {
                                wire_health.record_failure(chain_id);
                                last_err = e;
                            }
                        }
                    }
                    // INVARIANT 3+2: group exhausted — RST, no native TCP.
                    return Err(last_err);
                }
                // Default: legacy, try all (still no native TCP on exhaustion).
                SocksFailurePolicy::Default => {
                    let mut last_err = primary_err;
                    for (chain_id, fo_pool) in failover {
                        if !wire_health.is_healthy(chain_id) {
                            continue;
                        }
                        match fo_pool.lease().await {
                            Ok(l) => {
                                wire_health.record_success(chain_id);
                                return Ok(l);
                            }
                            Err(e) => {
                                wire_health.record_failure(chain_id);
                                last_err = e;
                            }
                        }
                    }
                    // INVARIANT 2: no native TCP on exhaustion.
                    return Err(last_err);
                }
            }
        }
    }
}

/// Read and discard a SOCKS5 address field (atyp + addr bytes + port).
/// Used for UDP ASSOCIATE where client sends its own address which we ignore.
async fn consume_socks5_addr(atyp: u8, stream: &mut TcpStream) -> Result<(), MirageError> {
    match atyp {
        SOCKS5_ATYP_IPV4 => {
            let mut buf = [0u8; 6]; // 4 addr + 2 port
            stream.read_exact(&mut buf).await?;
        }
        SOCKS5_ATYP_DOMAIN => {
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let domain_len = len_buf[0] as usize;
            let mut buf = vec![0u8; domain_len + 2]; // domain + 2 port
            stream.read_exact(&mut buf).await?;
        }
        SOCKS5_ATYP_IPV6 => {
            let mut buf = [0u8; 18]; // 16 addr + 2 port
            stream.read_exact(&mut buf).await?;
        }
        _ => {
            return Err(MirageError::Protocol(format!(
                "SOCKS5 unsupported atyp: 0x{atyp:02x}"
            )));
        }
    }
    Ok(())
}

/// Handle a single SOCKS5 client connection.
async fn handle_socks5_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    pool: Arc<TunnelPool>,
    mux_pool: Option<Arc<MuxPool>>,
    router: Arc<RoutingTable>,
    failover: Arc<Vec<(String, Arc<TunnelPool>)>>,
    chain_router: Option<Arc<dyn Router>>,
    all_pools: Arc<HashMap<ChainId, Arc<TunnelPool>>>,
    chain_policies: Arc<HashMap<String, SocksFailurePolicy>>,
    wire_health: Arc<WireHealthMap>,
    primary_chain_id: Arc<String>,
) -> Result<(), MirageError> {
    // ── 1. SOCKS5 greeting ────────────────────────────────────────────────
    // Client: [0x05, n_methods, method1, ...]
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await?;

    if header[0] != SOCKS5_VERSION {
        return Err(MirageError::Protocol(format!(
            "expected SOCKS5 version 5, got {}",
            header[0]
        )));
    }

    let n_methods = header[1] as usize;
    if n_methods == 0 {
        return Err(MirageError::Protocol("SOCKS5 greeting: zero methods".into()));
    }

    let mut methods = vec![0u8; n_methods];
    stream.read_exact(&mut methods).await?;

    // Reply: [0x05, 0x00] — no authentication required.
    stream.write_all(&[SOCKS5_VERSION, SOCKS5_AUTH_NONE]).await?;

    // ── 2. SOCKS5 request ─────────────────────────────────────────────────
    // Client: [0x05, cmd, 0x00, atyp, ...addr, port_hi, port_lo]
    let mut req_header = [0u8; 4];
    stream.read_exact(&mut req_header).await?;

    if req_header[0] != SOCKS5_VERSION {
        return Err(MirageError::Protocol("SOCKS5 request: bad version".into()));
    }

    let cmd = req_header[1];
    let atyp = req_header[3];

    match cmd {
        SOCKS5_CMD_CONNECT => {
            // ── 3. Parse destination address ──────────────────────────────────────
            let dest = match parse_destination(&mut stream, atyp).await {
                Ok(d) => d,
                Err(e) => {
                    send_socks5_error(&mut stream, SOCKS5_REP_ATYP_NOT_SUPPORTED).await;
                    return Err(e);
                }
            };

            // ── 4. Routing decision ───────────────────────────────────────────────
            let decision = match &dest {
                Destination::Domain(host, _) => router.decide_domain(host),
                Destination::Ip(addr) => router.decide_ip(&addr.ip()),
            };

            let target_addr = dest.to_addr_string();
            let decision_label = match &decision {
                RouteDecision::Direct => "direct",
                RouteDecision::Chain => "chain",
            };
            debug!(target = %target_addr, decision = decision_label, "routing decision");

            match decision {
                // ── INVARIANT 1: Bypass is sacred ────────────────────────────
                // RouteDecision::Direct means the routing table EXPLICITLY
                // designated this flow as bypass (clearnet is allowed).
                // This is the ONLY code path that may call route_direct.
                RouteDecision::Direct => route_direct(&mut stream, &target_addr).await,

                // ── INVARIANT 2 + 3: Wire flows must NOT reach native TCP ────
                // All RouteDecision::Chain flows are wire-designated.  On any
                // failure they receive a SOCKS5 error — never route_direct.
                RouteDecision::Chain => {
                    // Derive the failure policy for the primary chain of this server.
                    let policy = chain_policies
                        .get(primary_chain_id.as_str())
                        .cloned()
                        .unwrap_or(SocksFailurePolicy::Default);

                    if let Some(cr) = &chain_router {
                        // Router-driven path: pick ordered chain list, try each pool.
                        let target_key = TargetKey {
                            host: match &dest {
                                Destination::Domain(h, _) => h.clone(),
                                Destination::Ip(a) => a.ip().to_string(),
                            },
                            port: match &dest {
                                Destination::Domain(_, p) => *p,
                                Destination::Ip(a) => a.port(),
                            },
                            protocol_hint: ProtocolHint::Tcp,
                        };
                        let flow = FlowKey::Socks5 {
                            client_addr: peer,
                            request_id: 0,
                        };
                        let chain_ids = cr.pick(&target_key, &flow);
                        if chain_ids.is_empty() {
                            warn!(target = %target_addr, "router returned empty chain list — fail-closed");
                            // INVARIANT 2: pool exhausted on wire path → SOCKS5 error, NOT direct.
                            send_socks5_error(&mut stream, SOCKS5_REP_GENERAL_FAILURE).await;
                            return Err(MirageError::PoolExhausted);
                        }
                        route_via_router_pools(&mut stream, &chain_ids, &all_pools, &target_addr).await
                    } else {
                        // Legacy failover path (cli1 backward compat).
                        // Try mux first; on mux open_stream error fall back to TunnelPool.
                        if let Some(mp) = mux_pool {
                            match route_via_mux_pool(&mut stream, mp, &target_addr).await {
                                Ok(()) => Ok(()),
                                Err(e) if matches!(&e, MirageError::CircuitOpen) => {
                                    tracing::warn!(target = %target_addr, "mux circuit open — falling back to TunnelPool");
                                    route_via_pool_with_failover(
                                        &mut stream,
                                        pool,
                                        &primary_chain_id,
                                        &failover,
                                        &target_addr,
                                        &policy,
                                        &wire_health,
                                        &chain_policies,
                                    ).await
                                }
                                Err(e) => {
                                    tracing::warn!(target = %target_addr, error = %e, "mux open_stream failed — falling back to TunnelPool");
                                    route_via_pool_with_failover(
                                        &mut stream,
                                        pool,
                                        &primary_chain_id,
                                        &failover,
                                        &target_addr,
                                        &policy,
                                        &wire_health,
                                        &chain_policies,
                                    ).await
                                }
                            }
                        } else {
                            route_via_pool_with_failover(
                                &mut stream,
                                pool,
                                &primary_chain_id,
                                &failover,
                                &target_addr,
                                &policy,
                                &wire_health,
                                &chain_policies,
                            ).await
                        }
                    }
                }
            }
        }
        SOCKS5_CMD_UDP_ASSOCIATE => {
            // Consume client's declared address (RFC 1928 §4) — we ignore it.
            consume_socks5_addr(atyp, &mut stream).await?;
            handle_udp_associate(stream, pool, mux_pool, failover).await
        }
        _ => {
            send_socks5_error(&mut stream, SOCKS5_REP_CMD_NOT_SUPPORTED).await;
            Err(MirageError::Protocol(format!(
                "SOCKS5 unsupported command: 0x{cmd:02x}"
            )))
        }
    }
}

/// Route via an ordered list of ChainIds returned by `Router::pick`.
///
/// Tries each chain's pool in order; on lease failure or protocol error,
/// demotes that chain and tries the next. Returns `PoolExhausted` if no chain
/// succeeds.
async fn route_via_router_pools(
    client: &mut TcpStream,
    chain_ids: &[ChainId],
    all_pools: &HashMap<ChainId, Arc<TunnelPool>>,
    target_addr: &str,
) -> Result<(), MirageError> {
    for chain_id in chain_ids {
        let pool = match all_pools.get(chain_id) {
            Some(p) => Arc::clone(p),
            None => {
                warn!(chain = %chain_id, "router returned unknown chain — skipping");
                continue;
            }
        };
        match pool.lease().await {
            Err(e) => {
                warn!(chain = %chain_id, error = %e, "router path: lease failed — trying next chain");
                continue;
            }
            Ok(mut lease) => {
                let send_result = if lease.is_onion {
                    let target_bytes = target_addr.as_bytes();
                    let len_prefix = (target_bytes.len() as u16).to_be_bytes();
                    let mut framed = Vec::with_capacity(2 + target_bytes.len());
                    framed.extend_from_slice(&len_prefix);
                    framed.extend_from_slice(target_bytes);
                    lease.send(&framed).await
                } else {
                    lease.send(target_addr.as_bytes()).await
                };
                if let Err(e) = send_result {
                    warn!(chain = %chain_id, error = %e, "router path: target send failed — trying next chain");
                    lease.poison();
                    continue;
                }
                send_socks5_success(client).await?;
                lease.set_relay_active(true);
                let tunnel = lease.take_tunnel().ok_or(MirageError::TunnelClosed)?;
                let mut compat = TunnelCompat::wrap(tunnel);
                let result = relay_loop(client, &mut compat).await;
                lease.poison();
                return result;
            }
        }
    }
    Err(MirageError::PoolExhausted)
}

/// Connect directly to the target and relay data.
async fn route_direct(
    client: &mut TcpStream,
    target_addr: &str,
) -> Result<(), MirageError> {
    let mut target = TcpStream::connect(target_addr).await.map_err(|e| {
        MirageError::NodeUnreachable(format!("direct connect to {target_addr} failed: {e}"))
    })?;

    send_socks5_success(client).await?;

    tokio::io::copy_bidirectional(client, &mut target)
        .await
        .map_err(MirageError::Io)?;

    Ok(())
}

/// Lease a pre-warmed tunnel from the pool (with kill-switch-aware failover),
/// send the target address, then relay.
///
/// Primary pool is tried with `LEASE_SEND_MAX_ATTEMPTS` retries (absorbs
/// cold-pool / single-use-tunnel flakiness, #59).
///
/// On exhaustion the kill-switch policy governs what happens next:
///
/// - `FailClosed` (Strict): **no** failover — send SOCKS5 `0x01 General Failure`
///   reply to the client and return.  The flow goes NOWHERE.  Native TCP is
///   NEVER tried.  INVARIANT 3 (Strict).
///
/// - `Failover { group }` (General): try only failover pools whose `chain_id`
///   appears in `chain_policies` with a matching group name.  If all group
///   members fail → SOCKS5 error, **no** native TCP.  INVARIANT 3 (General).
///
/// - `Default` (no kill-switch): legacy behavior, try all failover pools in
///   priority order.  Still never calls `route_direct` — that is the
///   caller's responsibility (only `RouteDecision::Direct` may do that).
///   INVARIANT 2 preserved.
///
/// Wire health is consulted before attempting each pool: a chain marked
/// unhealthy (≥2 consecutive failures) is skipped without a lease attempt so
/// dead wires fail closed immediately instead of waiting 300–600 s for the
/// pool-lease timeout.
async fn route_via_pool_with_failover(
    client: &mut TcpStream,
    pool: Arc<TunnelPool>,
    primary_chain_id: &str,
    failover: &[(String, Arc<TunnelPool>)],
    target_addr: &str,
    policy: &SocksFailurePolicy,
    wire_health: &Arc<WireHealthMap>,
    chain_policies: &HashMap<String, SocksFailurePolicy>,
) -> Result<(), MirageError> {
    // ── S3: consult wire health before attempting primary ─────────────────────
    if !wire_health.is_healthy(primary_chain_id) {
        warn!(
            chain = %primary_chain_id,
            target = %target_addr,
            // INVARIANT 3: wire is known-dead — fail immediately without lease attempt.
            "wire-health: primary chain unhealthy, fail-closed without lease"
        );
        send_socks5_error(client, SOCKS5_REP_GENERAL_FAILURE).await;
        return Err(MirageError::PoolExhausted);
    }

    // ── Try the primary pool with bounded retry ───────────────────────────────
    let lease_result = lease_and_send_target(&pool, target_addr, LEASE_SEND_MAX_ATTEMPTS).await;
    let mut lease = match lease_result {
        Ok(l) => {
            // S3: record success — resets fail streak for this wire.
            wire_health.record_success(primary_chain_id);
            l
        }
        Err(primary_err) => {
            // S3: record failure — may mark wire unhealthy after threshold.
            wire_health.record_failure(primary_chain_id);

            match policy {
                // ── INVARIANT 3 / STRICT: fail-closed, no reroute ────────────
                SocksFailurePolicy::FailClosed => {
                    warn!(
                        chain = %primary_chain_id,
                        error = %primary_err,
                        // INVARIANT 3: Strict mode — no failover, no native TCP.
                        "kill-switch FailClosed: primary failed, refusing flow"
                    );
                    send_socks5_error(client, SOCKS5_REP_GENERAL_FAILURE).await;
                    return Err(primary_err);
                }

                // ── INVARIANT 3 / GENERAL: group-only failover ───────────────
                SocksFailurePolicy::Failover { group } => {
                    warn!(
                        chain = %primary_chain_id,
                        error = %primary_err,
                        group = %group,
                        "kill-switch Failover: primary failed, trying group members"
                    );
                    let mut last_err = primary_err;
                    let mut found: Option<crate::pool::TunnelLease> = None;

                    for (chain_id, fo_pool) in failover {
                        // Only try chains that are members of the same group.
                        let chain_policy = chain_policies
                            .get(chain_id)
                            .cloned()
                            .unwrap_or(SocksFailurePolicy::Default);
                        let in_group = matches!(
                            &chain_policy,
                            SocksFailurePolicy::Failover { group: g } if g == group
                        );
                        if !in_group {
                            debug!(
                                chain = %chain_id,
                                group = %group,
                                // INVARIANT 3: chain not in this group — skip.
                                "kill-switch Failover: skipping chain not in group"
                            );
                            continue;
                        }
                        // S3: skip wire-health-unhealthy group members.
                        if !wire_health.is_healthy(chain_id) {
                            warn!(
                                chain = %chain_id,
                                // S3: wire-health says dead — instant skip.
                                "wire-health: failover group member unhealthy, skipping"
                            );
                            continue;
                        }
                        warn!(chain = %chain_id, "kill-switch Failover: trying group member");
                        match lease_and_send_target(fo_pool, target_addr, 1).await {
                            Ok(l) => {
                                wire_health.record_success(chain_id);
                                info!(chain = %chain_id, "kill-switch Failover: using group member");
                                found = Some(l);
                                break;
                            }
                            Err(e) => {
                                wire_health.record_failure(chain_id);
                                warn!(chain = %chain_id, error = %e, "kill-switch Failover: group member failed");
                                last_err = e;
                            }
                        }
                    }

                    match found {
                        Some(l) => l,
                        None => {
                            // INVARIANT 3: all group members exhausted → fail-closed.
                            // INVARIANT 2: never route_direct here.
                            warn!(
                                group = %group,
                                "kill-switch Failover: all group members exhausted, fail-closed"
                            );
                            send_socks5_error(client, SOCKS5_REP_GENERAL_FAILURE).await;
                            return Err(last_err);
                        }
                    }
                }

                // ── Default: legacy failover (no kill-switch configured) ─────
                // INVARIANT 2: even in legacy mode we NEVER call route_direct —
                // this function is only called from RouteDecision::Chain.
                SocksFailurePolicy::Default => {
                    warn!(error = %primary_err, "primary pool exhausted after retries, trying failover (default policy)");
                    let mut last_err = primary_err;
                    let mut found: Option<crate::pool::TunnelLease> = None;

                    for (chain_id, fo_pool) in failover {
                        // S3: skip unhealthy chains even in default mode.
                        if !wire_health.is_healthy(chain_id) {
                            warn!(
                                chain = %chain_id,
                                "wire-health: failover chain unhealthy, skipping"
                            );
                            continue;
                        }
                        warn!(chain = %chain_id, "failing over to chain (default policy)");
                        match lease_and_send_target(fo_pool, target_addr, 1).await {
                            Ok(l) => {
                                wire_health.record_success(chain_id);
                                info!(chain = %chain_id, "failover: using chain");
                                found = Some(l);
                                break;
                            }
                            Err(e) => {
                                wire_health.record_failure(chain_id);
                                warn!(chain = %chain_id, error = %e, "failover chain also failed");
                                last_err = e;
                            }
                        }
                    }
                    // INVARIANT 2: on exhaustion return error — do NOT route_direct.
                    match found {
                        Some(l) => l,
                        None => return Err(last_err),
                    }
                }
            }
        }
    };

    send_socks5_success(client).await?;

    lease.set_relay_active(true);
    let tunnel = lease.take_tunnel().ok_or(MirageError::TunnelClosed)?;
    let mut compat = TunnelCompat::wrap(tunnel);
    let result = relay_loop(client, &mut compat).await;
    lease.poison();
    result
}

/// Open a mux stream, send the target address, then relay.
/// Try to route a SOCKS5 CONNECT through the mux pool.
///
/// Returns Err **without sending anything to the client** if mux setup fails
/// (open_stream / target write). This lets the caller fall back to the raw
/// TunnelPool. Only after the target write succeeds do we send SOCKS5 success
/// and enter the relay loop — at that point any further error is unrecoverable
/// (the client already saw success, can't re-route).
async fn route_via_mux_pool(
    client: &mut TcpStream,
    mux_pool: Arc<MuxPool>,
    target_addr: &str,
) -> Result<(), MirageError> {
    let is_onion = mux_pool.is_onion.load(std::sync::atomic::Ordering::Acquire);
    let mut stream = mux_pool.open_stream().await?;

    let write_result = if is_onion {
        let target_bytes = target_addr.as_bytes();
        let len_prefix = (target_bytes.len() as u16).to_be_bytes();
        let mut framed = Vec::with_capacity(2 + target_bytes.len());
        framed.extend_from_slice(&len_prefix);
        framed.extend_from_slice(target_bytes);
        tokio::io::AsyncWriteExt::write_all(&mut stream, &framed).await
    } else {
        tokio::io::AsyncWriteExt::write_all(&mut stream, target_addr.as_bytes()).await
    };
    if let Err(e) = write_result {
        return Err(MirageError::Io(e));
    }

    send_socks5_success(client).await?;

    relay_loop(client, &mut stream).await
}

/// Send a SOCKS5 success reply with a zeroed bind address and port.
async fn send_socks5_success(stream: &mut TcpStream) -> Result<(), MirageError> {
    // [version, rep=success, rsv, atyp=IPv4, 0.0.0.0, port=0]
    stream
        .write_all(&[
            SOCKS5_VERSION,
            SOCKS5_REP_SUCCESS,
            0x00,
            SOCKS5_ATYP_IPV4,
            0, 0, 0, 0,
            0, 0,
        ])
        .await?;
    Ok(())
}

/// Handle a TCP connection that was transparently redirected by iptables.
#[cfg(target_os = "linux")]
///
/// The connection carries no SOCKS5 framing — the kernel already knows the
/// original destination and recorded it in `SO_ORIGINAL_DST`.  We recover
/// that address and route the connection through the tunnel pool just like a
/// normal SOCKS5 CONNECT would.
async fn handle_transparent_connection(
    mut stream: TcpStream,
    pool: Arc<TunnelPool>,
    mux_pool: Option<Arc<MuxPool>>,
    router: Arc<RoutingTable>,
    failover: Arc<Vec<(String, Arc<TunnelPool>)>>,
    chain_router: Option<Arc<dyn Router>>,
    all_pools: Arc<HashMap<ChainId, Arc<TunnelPool>>>,
    chain_policies: Arc<HashMap<String, SocksFailurePolicy>>,
    wire_health: Arc<WireHealthMap>,
    primary_chain_id: Arc<String>,
) -> Result<(), MirageError> {
    let orig_dst = get_original_dst(&stream)?;

    let decision = router.decide_ip(&orig_dst.ip());
    let target_addr = orig_dst.to_string();
    let decision_label = match &decision {
        RouteDecision::Direct => "direct",
        RouteDecision::Chain => "chain",
    };
    debug!(target = %target_addr, decision = decision_label, "transparent routing decision");

    match decision {
        // ── INVARIANT 1: Bypass sacred — transparent direct connect ──────────
        // RouteDecision::Direct = routing table designated this as bypass.
        // Only this arm may connect via native TCP.
        RouteDecision::Direct => {
            let mut target = TcpStream::connect(&target_addr).await.map_err(|e| {
                MirageError::NodeUnreachable(format!(
                    "transparent direct connect to {target_addr} failed: {e}"
                ))
            })?;
            tokio::io::copy_bidirectional(&mut stream, &mut target)
                .await
                .map_err(MirageError::Io)?;
        }

        // ── INVARIANT 2 + 3: Wire flows must NOT reach native TCP ────────────
        RouteDecision::Chain => {
            if let Some(cr) = &chain_router {
                // Router-driven transparent path.
                let target_key = TargetKey {
                    host: orig_dst.ip().to_string(),
                    port: orig_dst.port(),
                    protocol_hint: ProtocolHint::Tcp,
                };
                let flow = FlowKey::TunFlow {
                    five_tuple_hash: {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        orig_dst.hash(&mut h);
                        h.finish()
                    },
                };
                let chain_ids = cr.pick(&target_key, &flow);
                if chain_ids.is_empty() {
                    // INVARIANT 2: router exhausted — RST the connection, no native TCP.
                    return Err(MirageError::PoolExhausted);
                }
                route_via_router_pools_transparent(&mut stream, &chain_ids, &all_pools, &target_addr).await?;
            } else if let Some(mp) = mux_pool {
                route_via_mux_pool(&mut stream, mp, &target_addr).await?;
            } else {
                // Derive policy for transparent path (same as SOCKS5 path).
                let policy = chain_policies
                    .get(primary_chain_id.as_str())
                    .cloned()
                    .unwrap_or(SocksFailurePolicy::Default);

                // S3: consult wire health before attempting primary.
                if !wire_health.is_healthy(primary_chain_id.as_str()) {
                    warn!(
                        chain = %primary_chain_id,
                        // INVARIANT 3: wire known-dead — RST, no native TCP.
                        "wire-health: primary chain unhealthy, transparent fail-closed"
                    );
                    return Err(MirageError::PoolExhausted);
                }

                let lease_result = lease_with_failover_policy(
                    &pool,
                    &primary_chain_id,
                    &failover,
                    &policy,
                    &wire_health,
                    &chain_policies,
                ).await;
                let mut lease = match lease_result {
                    Ok(l) => l,
                    Err(e) => {
                        // INVARIANT 2: exhausted — RST, no native TCP.
                        return Err(e);
                    }
                };

                let send_result = if lease.is_onion {
                    let target_bytes = target_addr.as_bytes();
                    let len_prefix = (target_bytes.len() as u16).to_be_bytes();
                    let mut framed = Vec::with_capacity(2 + target_bytes.len());
                    framed.extend_from_slice(&len_prefix);
                    framed.extend_from_slice(target_bytes);
                    lease.send(&framed).await
                } else {
                    lease.send(target_addr.as_bytes()).await
                };
                if let Err(e) = send_result {
                    lease.poison();
                    return Err(e);
                }
                lease.set_relay_active(true);
                let tunnel = lease.take_tunnel().ok_or(MirageError::TunnelClosed)?;
                let mut compat = TunnelCompat::wrap(tunnel);
                let result = relay_loop(&mut stream, &mut compat).await;
                lease.poison();
                result?;
            }
        }
    }

    Ok(())
}

/// Router-driven transparent proxy path (no SOCKS5 success reply needed).
#[cfg(target_os = "linux")]
async fn route_via_router_pools_transparent(
    client: &mut TcpStream,
    chain_ids: &[ChainId],
    all_pools: &HashMap<ChainId, Arc<TunnelPool>>,
    target_addr: &str,
) -> Result<(), MirageError> {
    for chain_id in chain_ids {
        let pool = match all_pools.get(chain_id) {
            Some(p) => Arc::clone(p),
            None => {
                warn!(chain = %chain_id, "transparent router: unknown chain — skipping");
                continue;
            }
        };
        match pool.lease().await {
            Err(e) => {
                warn!(chain = %chain_id, error = %e, "transparent router: lease failed — trying next");
                continue;
            }
            Ok(mut lease) => {
                let send_result = if lease.is_onion {
                    let target_bytes = target_addr.as_bytes();
                    let len_prefix = (target_bytes.len() as u16).to_be_bytes();
                    let mut framed = Vec::with_capacity(2 + target_bytes.len());
                    framed.extend_from_slice(&len_prefix);
                    framed.extend_from_slice(target_bytes);
                    lease.send(&framed).await
                } else {
                    lease.send(target_addr.as_bytes()).await
                };
                if let Err(e) = send_result {
                    warn!(chain = %chain_id, error = %e, "transparent router: target send failed — next");
                    lease.poison();
                    continue;
                }
                lease.set_relay_active(true);
                let tunnel = lease.take_tunnel().ok_or(MirageError::TunnelClosed)?;
                let mut compat = TunnelCompat::wrap(tunnel);
                let result = relay_loop(client, &mut compat).await;
                lease.poison();
                return result;
            }
        }
    }
    Err(MirageError::PoolExhausted)
}

/// Parse the SOCKS5 destination address from `stream` given `atyp`.
///
/// Reads the address and port fields according to RFC 1928 §5.
/// Returns a [`Destination`] on success.
async fn parse_destination(stream: &mut TcpStream, atyp: u8) -> Result<Destination, MirageError> {
    match atyp {
        SOCKS5_ATYP_IPV4 => {
            let mut addr_buf = [0u8; 4];
            stream.read_exact(&mut addr_buf).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            let port = u16::from_be_bytes(port_buf);
            let ip = std::net::Ipv4Addr::from(addr_buf);
            Ok(Destination::Ip(SocketAddr::from((ip, port))))
        }
        SOCKS5_ATYP_DOMAIN => {
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let domain_len = len_buf[0] as usize;
            let mut domain_buf = vec![0u8; domain_len];
            stream.read_exact(&mut domain_buf).await?;
            let domain = String::from_utf8(domain_buf).map_err(|_| {
                MirageError::Protocol("SOCKS5 domain is not valid UTF-8".into())
            })?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            Ok(Destination::Domain(domain, u16::from_be_bytes(port_buf)))
        }
        SOCKS5_ATYP_IPV6 => {
            let mut addr_buf = [0u8; 16];
            stream.read_exact(&mut addr_buf).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            let port = u16::from_be_bytes(port_buf);
            let ip = std::net::Ipv6Addr::from(addr_buf);
            Ok(Destination::Ip(SocketAddr::from((ip, port))))
        }
        _ => Err(MirageError::Protocol(format!(
            "SOCKS5 unsupported atyp: 0x{atyp:02x}"
        ))),
    }
}

/// Send a SOCKS5 error reply (best-effort — errors are ignored).
async fn send_socks5_error(stream: &mut TcpStream, rep: u8) {
    let _ = stream
        .write_all(&[
            SOCKS5_VERSION,
            rep,
            0x00,
            SOCKS5_ATYP_IPV4,
            0, 0, 0, 0,
            0, 0,
        ])
        .await;
}

// ── UDP ASSOCIATE (RFC 1928 §7) ───────────────────────────────────────────────

/// Handle SOCKS5 CMD 0x03 UDP ASSOCIATE.
///
/// Binds a local UDP socket, replies with its address, then relays UDP
/// datagrams between the TUN client and the exit node via the tunnel pool.
/// The TCP connection is kept alive as a keepalive signal — when it closes
/// the relay is torn down.
async fn handle_udp_associate(
    mut tcp_stream: TcpStream,
    pool: Arc<TunnelPool>,
    mux_pool: Option<Arc<MuxPool>>,
    failover: Arc<Vec<(String, Arc<TunnelPool>)>>,
) -> Result<(), MirageError> {
    let udp_sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(MirageError::Io)?;
    let local_addr = udp_sock.local_addr().map_err(MirageError::Io)?;

    send_socks5_udp_success(&mut tcp_stream, local_addr).await?;

    info!(udp_addr = %local_addr, "SOCKS5 UDP ASSOCIATE: relay socket bound");

    let udp_sock = Arc::new(udp_sock);
    let cancel = CancellationToken::new();

    let relay_cancel = cancel.clone();
    let relay_udp = Arc::clone(&udp_sock);
    let relay_handle = tokio::spawn(async move {
        if let Err(e) = udp_associate_relay(relay_udp, mux_pool, pool, failover, relay_cancel).await {
            debug!("UDP ASSOCIATE relay ended: {e}");
        }
    });

    // TCP keepalive: wait for client to close TCP connection.
    let mut keepalive_buf = [0u8; 1];
    let _ = tcp_stream.read(&mut keepalive_buf).await;

    cancel.cancel();
    let _ = relay_handle.await;

    debug!("SOCKS5 UDP ASSOCIATE: session ended");
    Ok(())
}

/// Core UDP relay loop.
///
/// Receives SOCKS5 §7 datagrams from the TUN-side UDP socket, strips the
/// header, routes each destination's payload through a dedicated tunnel
/// session, and sends responses back wrapped in §7 headers.
async fn udp_associate_relay(
    udp_sock: Arc<tokio::net::UdpSocket>,
    mux_pool: Option<Arc<MuxPool>>,
    pool: Arc<TunnelPool>,
    failover: Arc<Vec<(String, Arc<TunnelPool>)>>,
    cancel: CancellationToken,
) -> Result<(), MirageError> {
    let mut buf = vec![0u8; 65535];
    // Map: destination SocketAddr → sender channel for that session's tunnel.
    let mut sessions: HashMap<SocketAddr, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel::<(Vec<u8>, SocketAddr)>();
    let mut client_addr: Option<SocketAddr> = None;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,

            result = udp_sock.recv_from(&mut buf) => {
                let (n, from) = result.map_err(MirageError::Io)?;
                if client_addr.is_none() {
                    client_addr = Some(from);
                }

                let data = &buf[..n];
                // RFC 1928 §7: RSV(2) FRAG(1) ATYP(1) DST.ADDR DST.PORT DATA
                if data.len() < 4 {
                    continue;
                }
                let frag = data[2];
                if frag != 0 {
                    // Fragmentation not supported — drop.
                    continue;
                }

                let (header_len, dst) = match parse_udp_request_header(&data[3..]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let payload_start = 3 + header_len;
                if payload_start > n {
                    continue;
                }
                let payload = &data[payload_start..n];

                if !sessions.contains_key(&dst) {
                    let (tx, rx) = mpsc::channel(64);
                    sessions.insert(dst, tx);

                    let target = format!("udp:{dst}");
                    let mux = mux_pool.clone();
                    let p = Arc::clone(&pool);
                    let fo = Arc::clone(&failover);
                    let resp = resp_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = udp_session_task(target, mux, p, fo, rx, resp, dst).await {
                            debug!("UDP session {dst} ended: {e}");
                        }
                    });
                }

                if let Some(tx) = sessions.get(&dst) {
                    let _ = tx.send(payload.to_vec()).await;
                }
            }

            resp = resp_rx.recv() => {
                if let Some((payload, src_dst)) = resp {
                    if let Some(client) = client_addr {
                        let mut packet = build_udp_response_header(src_dst);
                        packet.extend_from_slice(&payload);
                        let _ = udp_sock.send_to(&packet, client).await;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Per-destination tunnel session: forwards payloads through a mux stream or
/// raw tunnel and sends responses back via `resp_tx`.
async fn udp_session_task(
    target: String,
    mux_pool: Option<Arc<MuxPool>>,
    pool: Arc<TunnelPool>,
    failover: Arc<Vec<(String, Arc<TunnelPool>)>>,
    mut rx: mpsc::Receiver<Vec<u8>>,
    resp_tx: mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>,
    dst: SocketAddr,
) -> Result<(), MirageError> {
    const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

    if let Some(mp) = mux_pool {
        let is_onion = mp.is_onion.load(std::sync::atomic::Ordering::Acquire);
        let mut stream = mp.open_stream().await?;

        if is_onion {
            let target_bytes = target.as_bytes();
            let len_prefix = (target_bytes.len() as u16).to_be_bytes();
            tokio::io::AsyncWriteExt::write_all(&mut stream, &len_prefix)
                .await
                .map_err(MirageError::Io)?;
            tokio::io::AsyncWriteExt::write_all(&mut stream, target_bytes)
                .await
                .map_err(MirageError::Io)?;
        } else {
            tokio::io::AsyncWriteExt::write_all(&mut stream, target.as_bytes())
                .await
                .map_err(MirageError::Io)?;
        }

        let (mut read_half, mut write_half) = tokio::io::split(stream);

        let resp_tx2 = resp_tx.clone();
        let read_handle = tokio::spawn(async move {
            let mut frame_buf = Vec::with_capacity(2048);
            loop {
                match read_udp_frame(&mut read_half, &mut frame_buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = resp_tx2.send((frame_buf[..n].to_vec(), dst));
                    }
                }
            }
        });

        loop {
            match tokio::time::timeout(IDLE_TIMEOUT, rx.recv()).await {
                Ok(Some(payload)) => {
                    if write_udp_frame(&mut write_half, &payload).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }

        read_handle.abort();
    } else {
        let mut lease = lease_with_failover(&pool, &failover).await?;

        let send_result = if lease.is_onion {
            let target_bytes = target.as_bytes();
            let len_prefix = (target_bytes.len() as u16).to_be_bytes();
            let mut framed = Vec::with_capacity(2 + target_bytes.len());
            framed.extend_from_slice(&len_prefix);
            framed.extend_from_slice(target_bytes);
            lease.send(&framed).await
        } else {
            lease.send(target.as_bytes()).await
        };

        if let Err(e) = send_result {
            lease.poison();
            return Err(e);
        }

        lease.set_relay_active(true);
        let tunnel = lease.take_tunnel().ok_or(MirageError::TunnelClosed)?;
        let compat = TunnelCompat::wrap(tunnel);
        let (mut read_half, mut write_half) = tokio::io::split(compat);

        let resp_tx2 = resp_tx.clone();
        let read_handle = tokio::spawn(async move {
            let mut frame_buf = Vec::with_capacity(2048);
            loop {
                match read_udp_frame(&mut read_half, &mut frame_buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = resp_tx2.send((frame_buf[..n].to_vec(), dst));
                    }
                }
            }
        });

        loop {
            match tokio::time::timeout(IDLE_TIMEOUT, rx.recv()).await {
                Ok(Some(payload)) => {
                    if write_udp_frame(&mut write_half, &payload).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }

        read_handle.abort();
        lease.poison();
    }

    Ok(())
}

/// Parse SOCKS5 §7 UDP request header starting at the ATYP byte.
/// Returns `(header_length, destination_addr)`.
fn parse_udp_request_header(data: &[u8]) -> Result<(usize, SocketAddr), MirageError> {
    if data.is_empty() {
        return Err(MirageError::Protocol("UDP header: empty".into()));
    }
    let atyp = data[0];
    match atyp {
        SOCKS5_ATYP_IPV4 => {
            if data.len() < 7 {
                return Err(MirageError::Protocol("UDP header: IPv4 too short".into()));
            }
            let ip = std::net::Ipv4Addr::new(data[1], data[2], data[3], data[4]);
            let port = u16::from_be_bytes([data[5], data[6]]);
            Ok((7, SocketAddr::from((ip, port))))
        }
        SOCKS5_ATYP_IPV6 => {
            if data.len() < 19 {
                return Err(MirageError::Protocol("UDP header: IPv6 too short".into()));
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&data[1..17]);
            let ip = std::net::Ipv6Addr::from(octets);
            let port = u16::from_be_bytes([data[17], data[18]]);
            Ok((19, SocketAddr::from((ip, port))))
        }
        SOCKS5_ATYP_DOMAIN => {
            // Domain-based destinations won't appear from TUN (raw IPs only).
            warn!("UDP ASSOCIATE: domain-based destination not supported, dropping");
            Err(MirageError::Protocol("UDP header: domain not supported".into()))
        }
        _ => Err(MirageError::Protocol(format!(
            "UDP header: unknown atyp 0x{atyp:02x}"
        ))),
    }
}

/// Build SOCKS5 §7 UDP response header for wrapping a reply to the client.
fn build_udp_response_header(src: SocketAddr) -> Vec<u8> {
    let mut header = Vec::with_capacity(10);
    header.extend_from_slice(&[0x00, 0x00, 0x00]); // RSV RSV FRAG=0
    match src {
        SocketAddr::V4(v4) => {
            header.push(SOCKS5_ATYP_IPV4);
            header.extend_from_slice(&v4.ip().octets());
            header.extend_from_slice(&v4.port().to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            header.push(SOCKS5_ATYP_IPV6);
            header.extend_from_slice(&v6.ip().octets());
            header.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
    header
}

/// Send SOCKS5 UDP ASSOCIATE success reply with the bound UDP socket address.
pub(crate) async fn send_socks5_udp_success(
    stream: &mut TcpStream,
    addr: SocketAddr,
) -> Result<(), MirageError> {
    let mut reply = Vec::with_capacity(10);
    reply.extend_from_slice(&[SOCKS5_VERSION, SOCKS5_REP_SUCCESS, 0x00]);
    match addr {
        SocketAddr::V4(v4) => {
            reply.push(SOCKS5_ATYP_IPV4);
            reply.extend_from_slice(&v4.ip().octets());
            reply.extend_from_slice(&v4.port().to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            reply.push(SOCKS5_ATYP_IPV6);
            reply.extend_from_slice(&v6.ip().octets());
            reply.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
    stream.write_all(&reply).await?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};

    use tokio::io::AsyncWriteExt as _;

    use super::{
        build_udp_response_header, parse_destination, parse_udp_request_header,
        SOCKS5_ATYP_IPV4, SOCKS5_ATYP_IPV6,
    };

    /// Create a connected loopback TCP pair.  Returns `(writer, reader)`.
    async fn tcp_pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = tokio::net::TcpStream::connect(addr);
        let accept = listener.accept();
        let (writer, accept_result) = tokio::join!(connect, accept);
        let (reader, _) = accept_result.unwrap();
        (writer.unwrap(), reader)
    }

    /// SOCKS5 CONNECT ATYP=4 (IPv6): 16-byte addr + 2-byte port.
    #[tokio::test]
    async fn parse_destination_ipv6_atyp4() {
        let (mut writer, mut reader) = tcp_pair().await;

        // Write: 16 bytes of IPv6 addr + 2 bytes port (big-endian).
        let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let port: u16 = 4700;
        writer.write_all(&ip.octets()).await.unwrap();
        writer.write_all(&port.to_be_bytes()).await.unwrap();
        drop(writer);

        let dest = parse_destination(&mut reader, SOCKS5_ATYP_IPV6)
            .await
            .expect("parse_destination ATYP=4 must succeed");

        match dest {
            super::Destination::Ip(addr) => {
                assert_eq!(addr.port(), port);
                match addr {
                    SocketAddr::V6(v6) => assert_eq!(*v6.ip(), ip),
                    SocketAddr::V4(_) => panic!("expected V6 address"),
                }
            }
            super::Destination::Domain(_, _) => panic!("expected Ip destination"),
        }
    }

    /// SOCKS5 CONNECT ATYP=1 (IPv4) still works after the IPv6 arm is present.
    #[tokio::test]
    async fn parse_destination_ipv4_atyp1_still_works() {
        let (mut writer, mut reader) = tcp_pair().await;

        let ip: std::net::Ipv4Addr = "192.168.1.1".parse().unwrap();
        let port: u16 = 80;
        writer.write_all(&ip.octets()).await.unwrap();
        writer.write_all(&port.to_be_bytes()).await.unwrap();
        drop(writer);

        let dest = parse_destination(&mut reader, SOCKS5_ATYP_IPV4)
            .await
            .expect("parse_destination ATYP=1 must succeed");

        match dest {
            super::Destination::Ip(addr) => {
                assert_eq!(addr.port(), port);
                match addr {
                    SocketAddr::V4(v4) => assert_eq!(*v4.ip(), ip),
                    SocketAddr::V6(_) => panic!("expected V4 address"),
                }
            }
            super::Destination::Domain(_, _) => panic!("expected Ip destination"),
        }
    }

    /// UDP request header ATYP=4 (IPv6) parses to correct addr + header length.
    #[test]
    fn parse_udp_request_header_ipv6() {
        let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let port: u16 = 9999;

        let mut data = vec![SOCKS5_ATYP_IPV6];
        data.extend_from_slice(&ip.octets());
        data.extend_from_slice(&port.to_be_bytes());

        let (hdr_len, addr) =
            parse_udp_request_header(&data).expect("parse_udp_request_header IPv6 must succeed");

        assert_eq!(hdr_len, 19); // 1 atyp + 16 addr + 2 port
        assert_eq!(addr.port(), port);
        match addr {
            SocketAddr::V6(v6) => assert_eq!(*v6.ip(), ip),
            SocketAddr::V4(_) => panic!("expected V6 address"),
        }
    }

    /// UDP reply header for a v6 src emits ATYP=4 + 16 bytes + 2-byte port.
    #[test]
    fn build_udp_response_header_ipv6() {
        let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let port: u16 = 9999;
        let src = SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0));

        let hdr = build_udp_response_header(src);

        // RSV(2) + FRAG(1) + ATYP(1) + 16 + 2 = 22 bytes
        assert_eq!(hdr.len(), 22);
        assert_eq!(hdr[0], 0x00); // RSV
        assert_eq!(hdr[1], 0x00); // RSV
        assert_eq!(hdr[2], 0x00); // FRAG=0
        assert_eq!(hdr[3], SOCKS5_ATYP_IPV6);
        assert_eq!(&hdr[4..20], &ip.octets());
        let parsed_port = u16::from_be_bytes([hdr[20], hdr[21]]);
        assert_eq!(parsed_port, port);
    }

    /// UDP reply header for a v4 src emits ATYP=1 + 4 bytes + 2-byte port.
    #[test]
    fn build_udp_response_header_ipv4() {
        let ip: std::net::Ipv4Addr = "10.0.0.1".parse().unwrap();
        let port: u16 = 1234;
        let src = SocketAddr::from((ip, port));

        let hdr = build_udp_response_header(src);

        assert_eq!(hdr.len(), 10); // RSV(2) + FRAG(1) + ATYP(1) + 4 + 2
        assert_eq!(hdr[3], SOCKS5_ATYP_IPV4);
        assert_eq!(&hdr[4..8], &ip.octets());
        let parsed_port = u16::from_be_bytes([hdr[8], hdr[9]]);
        assert_eq!(parsed_port, port);
    }

    // ── Router-driven pool failover tests ─────────────────────────────────────

    /// When `all_pools` is empty, `route_via_router_pools` returns `PoolExhausted`.
    ///
    /// This verifies that the router-driven path iterates the chain list and
    /// exhausts all candidates before returning an error, rather than crashing
    /// or silently succeeding.
    #[tokio::test]
    async fn router_path_exhausts_empty_pool_map() {
        use std::collections::HashMap;
        use std::sync::Arc;
        use mirage_core::error::MirageError;
        use tokio::net::TcpListener;

        use crate::{chain_set::ChainId, pool::TunnelPool};

        // Bind a real listener so we can connect to it.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Connect a fake "client" stream.
        let _accept_task = tokio::spawn(async move {
            // Just accept and drop immediately.
            let _ = listener.accept().await;
        });
        let mut client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        let chain_ids = vec![
            ChainId("chain-a".to_string()),
            ChainId("chain-b".to_string()),
        ];
        // Empty pool map — both chains will be skipped.
        let all_pools: HashMap<ChainId, Arc<TunnelPool>> = HashMap::new();

        let result = super::route_via_router_pools(
            &mut client_stream,
            &chain_ids,
            &all_pools,
            "example.com:443",
        )
        .await;

        assert!(
            matches!(result, Err(MirageError::PoolExhausted)),
            "expected PoolExhausted, got: {result:?}"
        );
    }
}
