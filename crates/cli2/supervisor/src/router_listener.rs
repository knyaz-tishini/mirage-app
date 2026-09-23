//! Supervisor-level SOCKS5 router listener.
//!
//! Binds a single SOCKS5 port. For each CONNECT request it picks a worker's
//! SOCKS5 port using the configured policy (currently round-robin), then
//! forwards the connection there via a second SOCKS5 CONNECT. The two-hop
//! SOCKS5 relay is transparent to the client.
//!
//! This is the Wave 2b router integration point for cli2. It does NOT require
//! `ChainSet` or `TunnelPool` — those live in the workers. The supervisor just
//! dispatches by address.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use mirage_client_core::{socks5::SocksFailurePolicy, wire_health::WireHealthMap};
use mirage_core::config::WireNature;
use mirage_core::local_endpoint::LocalChannel;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing::{debug, error, info, warn};

// ── SOCKS5 constants (RFC 1928) ───────────────────────────────────────────────

const VER: u8 = 0x05;
const AUTH_NONE: u8 = 0x00;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;
const REP_SUCCESS: u8 = 0x00;
const REP_GENERAL: u8 = 0x01;
const REP_CMD_NOT_SUPPORTED: u8 = 0x07;

// ── Chain entry ───────────────────────────────────────────────────────────────

/// A worker's SOCKS5 dial target. Workstation mode: TCP loopback
/// (`chain.socks5_bind`). Box mode: a unix socket in the edge-client's own
/// private runtime directory (`config::worker_socks5_unix_path`) — never a
/// TCP port (owner ruling 2026-09-23: fleet boxes have no internal TCP
/// ports).
#[derive(Debug, Clone)]
pub enum WorkerSocksAddr {
    Tcp(SocketAddr),
    Unix(std::path::PathBuf),
}

impl std::fmt::Display for WorkerSocksAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerSocksAddr::Tcp(addr) => write!(f, "tcp:{addr}"),
            WorkerSocksAddr::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

/// One chain known to the supervisor, with its worker's SOCKS5 address.
#[derive(Debug, Clone)]
pub struct RouterChain {
    pub chain_id: String,
    pub socks5_addr: WorkerSocksAddr,
    /// Kill-switch failure policy for this chain (mirrors the per-worker
    /// gate in `client-core::socks5`). `Default` = legacy passthrough
    /// (unconditional round-robin, unchanged behavior).
    pub policy: SocksFailurePolicy,
    /// Structural wire nature (mirrors `mirage_core::config::ChainConfig::effective_nature()`).
    pub nature: WireNature,
}

// ── SupervisorRouter ─────────────────────────────────────────────────────────

/// Lightweight round-robin router over a live-updatable chain list.
///
/// Holds an ordered snapshot of `(chain_id, socks5_addr)` pairs behind an
/// [`arc_swap::ArcSwap`] — [`pick()`](Self::pick) loads the current snapshot
/// lock-free and clones the chains it needs, so a connection in flight is
/// never pinned to a stale generation and no lock is ever held across an
/// `.await`. On each `pick()` call it returns chains ordered starting from
/// the current cursor position (wrapping around). The caller iterates in
/// order, trying each worker's SOCKS5 until one succeeds.
///
/// `kill_switch = Strict` chains are excluded from the rotation pool at
/// construction/update time — a Strict chain's entire point is reachability
/// only via its own dedicated `socks5_bind` port; mixing it into the shared
/// rotation pool would defeat that pinning contract for every other flow
/// that might round-robin onto it.
///
/// # Why the chain list is swappable, not fixed at construction
///
/// Before this, `chains` was a plain `Vec` set once in [`new`](Self::new) and
/// never touched again — a router bound in box mode before any chain had
/// started (`supervisor starting chains=0`, ae-other-172.86.76.34,
/// 2026-09-23 22:31:52Z) stayed empty for the rest of the process's life:
/// nothing on the config-reload path (`reconcile`) ever told it a chain had
/// since appeared. `update_chains` is the missing write side; the
/// config-reload call site is `reconcile`'s own doc.
pub struct SupervisorRouter {
    chains: arc_swap::ArcSwap<Vec<RouterChain>>,
    cursor: AtomicU64,
    /// Router-level per-wire liveness map, shared across all router
    /// connection handlers so a dead worker is detected instantly instead of
    /// waiting for the per-attempt handshake timeout on every connection.
    pub health: Arc<WireHealthMap>,
}

impl SupervisorRouter {
    /// Build from config chains list. `Strict`-policy chains are filtered out
    /// of the rotation pool (with a `warn!` per exclusion) — they remain
    /// reachable only via their own dedicated worker `socks5_bind`. `chains`
    /// may be empty (box mode binds before any chain exists) — [`pick()`](Self::pick)
    /// simply returns nothing until [`update_chains`](Self::update_chains) is
    /// next called with a non-empty set.
    pub fn new(chains: Vec<RouterChain>) -> Arc<Self> {
        Arc::new(Self {
            chains: arc_swap::ArcSwap::new(Arc::new(filter_strict(chains))),
            cursor: AtomicU64::new(0),
            health: WireHealthMap::new(),
        })
    }

    /// Replace the rotation pool with `chains` — same Strict-policy
    /// filtering as [`new`](Self::new). Called on every config reload
    /// (`reconcile`) so the router reflects the reloaded chain set with no
    /// process restart: a chain added, removed, or re-policied since the
    /// last reload takes effect on the very next `pick()`.
    pub fn update_chains(&self, chains: Vec<RouterChain>) {
        self.chains.store(Arc::new(filter_strict(chains)));
    }

    /// Return ordered chain list starting at the current round-robin cursor.
    ///
    /// Returns an empty vec when no chains are configured. Owned
    /// (`Vec<RouterChain>`, not `Vec<&RouterChain>`): the snapshot loaded via
    /// `ArcSwap::load_full` may be superseded by a concurrent
    /// `update_chains` call at any point during the caller's (long-lived,
    /// `.await`-spanning) connection handling, so nothing here borrows from
    /// it past this function returning.
    pub fn pick(&self) -> Vec<RouterChain> {
        let chains = self.chains.load_full();
        if chains.is_empty() {
            return Vec::new();
        }
        let n = chains.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) as usize % n;
        let mut result = Vec::with_capacity(n);
        for i in 0..n {
            result.push(chains[(start + i) % n].clone());
        }
        result
    }
}

/// Drop every `kill_switch = Strict` chain from `chains`, logging each
/// exclusion — shared by [`SupervisorRouter::new`] and
/// [`SupervisorRouter::update_chains`] so construction and every later
/// reload apply the identical rule.
fn filter_strict(chains: Vec<RouterChain>) -> Vec<RouterChain> {
    chains
        .into_iter()
        .filter(|c| {
            let strict = matches!(c.policy, SocksFailurePolicy::FailClosed);
            if strict {
                warn!(
                    chain = %c.chain_id,
                    "router: excluding Strict-policy chain from shared rotation pool \
                     — reachable only via its own dedicated socks5_bind"
                );
            }
            !strict
        })
        .collect()
}

/// Build the router's chain list from `config`: the workstation-mode TCP
/// `socks5_bind` per chain, or box mode's derived unix-socket path
/// (`crate::config::worker_socks5_unix_path`), gated by the optional
/// `routing.chain_ids` whitelist. Empty when `config.routing` is absent.
///
/// Shared by the startup bind in `main.rs` and every config reload
/// (`reconcile`) so the router's chain set is always derived the same way
/// regardless of when it runs — see [`SupervisorRouter`]'s own doc for the
/// incident this closes: a router built once at startup from this exact
/// logic, inlined, then never re-run.
pub fn build_router_chains(config: &crate::config::Config, box_mode: bool) -> Vec<RouterChain> {
    let Some(routing_cfg) = &config.routing else {
        return Vec::new();
    };
    let whitelist: Option<&Vec<String>> = routing_cfg.chain_ids.as_ref();
    config
        .chains
        .iter()
        .filter(|c| match whitelist {
            Some(ids) => ids.iter().any(|id| id == &c.id),
            None => true,
        })
        .filter_map(|c| {
            // Box mode: every worker binds a unix socket at a path the
            // supervisor derives itself — `Config::validate` already refused
            // this config if `c.socks5_bind` were set instead. Workstation
            // mode: the worker's TCP `socks5_bind`, as before.
            let socks5_addr = if box_mode {
                Some(WorkerSocksAddr::Unix(crate::config::worker_socks5_unix_path(&c.id)))
            } else {
                c.socks5_bind
                    .as_deref()
                    .and_then(|s| s.parse::<std::net::SocketAddr>().ok())
                    .map(WorkerSocksAddr::Tcp)
            };
            socks5_addr.map(|socks5_addr| RouterChain {
                chain_id: c.id.clone(),
                socks5_addr,
                policy: SocksFailurePolicy::from_chain_config(
                    c.kill_switch.as_ref(),
                    c.group.as_deref(),
                    &c.id,
                ),
                nature: c.nature,
            })
        })
        .collect()
}

// ── Router listener ───────────────────────────────────────────────────────────

/// Spawn the supervisor SOCKS5 router listener as a background task.
///
/// The task runs forever until the process exits.  Errors binding or accepting
/// are logged but do not kill the supervisor.
pub fn spawn_router_listener(bind: SocketAddr, router: Arc<SupervisorRouter>) {
    tokio::spawn(async move {
        match run_router_listener(bind, router).await {
            Ok(()) => {}
            Err(e) => error!("router listener exited with error: {e}"),
        }
    });
}

async fn run_router_listener(
    bind: SocketAddr,
    router: Arc<SupervisorRouter>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!(addr = %bind, "supervisor router SOCKS5 listener started");

    loop {
        let (stream, peer) = listener.accept().await?;
        debug!(%peer, "router: new connection");

        let router = Arc::clone(&router);
        tokio::spawn(async move {
            if let Err(e) = handle_router_connection(stream, &router).await {
                warn!(%peer, "router connection error: {e}");
            }
        });
    }
}

// ── Box mode: local-channel listener ──────────────────────────────────────────

/// Resolve `routing.listen_local_channel` to the [`LocalChannel`] it names.
///
/// Only `"edge-client-entrance"` — [`LocalChannel::EdgeClientEntrance`]'s own
/// stable label — resolves today. Any other string is `None`, and the caller
/// (`main.rs`) treats that as a hard config error: box mode never falls back
/// to the TCP `socks5_bind` listener, because fleet boxes have no internal
/// TCP ports (owner ruling 2026-09-23).
pub fn resolve_local_channel(name: &str) -> Option<LocalChannel> {
    match name {
        "edge-client-entrance" => Some(LocalChannel::EdgeClientEntrance),
        _ => None,
    }
}

/// Spawn the supervisor SOCKS5 router listener on a box-local channel —
/// box mode's counterpart to [`spawn_router_listener`]. The channel carries
/// the SAME SOCKS5 protocol as the TCP listener; only the transport changes
/// (unix socket on the fleet, loopback TCP on the Windows/test arm — see
/// [`LocalChannel::endpoint`]). The node's entrance dials in as the
/// [`LocalChannel::EdgeClientEntrance`] client and sends SOCKS5 CONNECT with
/// the subscriber's destination, exactly like a workstation SOCKS5 client
/// does against the TCP listener.
pub fn spawn_router_listener_local_channel(channel: LocalChannel, router: Arc<SupervisorRouter>) {
    tokio::spawn(async move {
        match run_router_listener_local_channel(channel, router).await {
            Ok(()) => {}
            Err(e) => error!(channel = channel.label(), "box-mode router listener exited with error: {e}"),
        }
    });
}

async fn run_router_listener_local_channel(
    channel: LocalChannel,
    router: Arc<SupervisorRouter>,
) -> std::io::Result<()> {
    let listener = channel.bind().await?;
    info!(channel = channel.label(), "supervisor router local-channel listener started (box mode)");

    loop {
        let (stream, peer) = listener.accept().await?;
        debug!(?peer, "router: new connection (box mode)");

        let router = Arc::clone(&router);
        tokio::spawn(async move {
            if let Err(e) = handle_router_connection(stream, &router).await {
                warn!(?peer, "router connection error: {e}");
            }
        });
    }
}

// ── Connection handler ────────────────────────────────────────────────────────

/// Handle one client connection and relay it through a worker's SOCKS5 port.
///
/// Generic over the client-facing stream so the SAME SOCKS5 parsing and
/// worker-forwarding logic serves both listeners: `TcpStream` for the
/// workstation-mode TCP router, `mirage_core::local_endpoint::LocalStream`
/// for the box-mode local-channel router. The wire protocol carried is
/// identical either way — only the transport underneath `client` differs.
async fn handle_router_connection<S>(mut client: S, router: &SupervisorRouter) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // ── 1. SOCKS5 greeting ─────────────────────────────────────────────────
    let mut hdr = [0u8; 2];
    client.read_exact(&mut hdr).await?;
    if hdr[0] != VER {
        return Err(std::io::Error::other(format!(
            "bad SOCKS5 version: {}",
            hdr[0]
        )));
    }
    let n = hdr[1] as usize;
    let mut methods = vec![0u8; n];
    client.read_exact(&mut methods).await?;
    client.write_all(&[VER, AUTH_NONE]).await?;

    // ── 2. SOCKS5 request ─────────────────────────────────────────────────
    let mut req = [0u8; 4];
    client.read_exact(&mut req).await?;
    if req[0] != VER {
        return Err(std::io::Error::other("bad SOCKS5 request version"));
    }
    if req[1] != CMD_CONNECT {
        write_socks5_error(&mut client, REP_CMD_NOT_SUPPORTED).await;
        return Err(std::io::Error::other("only CONNECT supported"));
    }

    let atyp = req[3];
    let (target_host, target_port) = read_socks5_target(&mut client, atyp).await?;

    // ── 3. Pick worker via router ─────────────────────────────────────────
    let candidates = router.pick();
    if candidates.is_empty() {
        write_socks5_error(&mut client, REP_GENERAL).await;
        return Err(std::io::Error::other("no workers available"));
    }

    // The round-robin start candidate anchors the group/nature gate for every
    // subsequent retry candidate — see §5/§6 of the same-nature killswitch plan.
    let first = &candidates[0];
    let first_group: Option<&str> = match &first.policy {
        SocksFailurePolicy::Failover { group } => Some(group.as_str()),
        _ => None,
    };

    for chain in &candidates {
        let eligible = match &chain.policy {
            // `Default` (kill_switch unset): legacy passthrough, unconditional —
            // preserves today's behavior exactly for undeclared chains.
            SocksFailurePolicy::Default => true,
            // `General{group}`: only a same-group, same-nature candidate may
            // stand in for the round-robin start candidate.
            SocksFailurePolicy::Failover { group } => {
                first_group == Some(group.as_str()) && chain.nature == first.nature
            }
            // `Strict` chains never reach the pool — already filtered out at
            // `SupervisorRouter::new` construction time.
            SocksFailurePolicy::FailClosed => false,
        };
        if !eligible {
            debug!(
                chain = %chain.chain_id,
                "router: skipping candidate — kill-switch group/nature gate"
            );
            continue;
        }

        debug!(chain = %chain.chain_id, worker = %chain.socks5_addr, "router: trying worker");
        match forward_via_worker(&mut client, &chain.socks5_addr, &target_host, target_port).await {
            Ok(()) => {
                router.health.record_success(&chain.chain_id);
                return Ok(());
            }
            Err(e) => {
                router.health.record_failure(&chain.chain_id);
                warn!(chain = %chain.chain_id, error = %e, "router: worker failed — trying next");
            }
        }
    }

    write_socks5_error(&mut client, REP_GENERAL).await;
    Err(std::io::Error::other("all workers exhausted"))
}

/// Read the SOCKS5 target address + port from the stream.
async fn read_socks5_target<S>(stream: &mut S, atyp: u8) -> std::io::Result<(String, u16)>
where
    S: AsyncRead + Unpin,
{
    match atyp {
        ATYP_IPV4 => {
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            let ip = std::net::Ipv4Addr::from(buf);
            let port = u16::from_be_bytes(port_buf);
            Ok((ip.to_string(), port))
        }
        ATYP_DOMAIN => {
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let len = len_buf[0] as usize;
            let mut domain = vec![0u8; len];
            stream.read_exact(&mut domain).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            let host = String::from_utf8(domain)
                .map_err(|_| std::io::Error::other("domain not UTF-8"))?;
            let port = u16::from_be_bytes(port_buf);
            Ok((host, port))
        }
        ATYP_IPV6 => {
            let mut buf = [0u8; 16];
            stream.read_exact(&mut buf).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            let ip = std::net::Ipv6Addr::from(buf);
            let port = u16::from_be_bytes(port_buf);
            Ok((format!("[{ip}]"), port))
        }
        _ => Err(std::io::Error::other(format!(
            "unsupported SOCKS5 atyp: 0x{atyp:02x}"
        ))),
    }
}

/// Worker handshake timeout — if the worker's upstream chain is dead, its
/// SOCKS5 handshake stalls indefinitely while the worker retries reconnect.
/// We bail fast and let the router try the next sibling.
const WORKER_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Forward the CONNECT to a worker's SOCKS5 port, then relay bidirectionally.
///
/// This implements a second SOCKS5 handshake to the worker, then splices data
/// between client and the upstream connection.
async fn forward_via_worker<S>(
    client: &mut S,
    worker_target: &WorkerSocksAddr,
    target_host: &str,
    target_port: u16,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match worker_target {
        WorkerSocksAddr::Tcp(addr) => {
            let addr = *addr;
            let handshake = async move {
                let mut worker = TcpStream::connect(addr).await?;
                worker.write_all(&[VER, 1, AUTH_NONE]).await?;
                let mut resp = [0u8; 2];
                worker.read_exact(&mut resp).await?;
                Ok::<_, std::io::Error>((worker, resp))
            };
            let (worker, resp) = tokio::time::timeout(WORKER_HANDSHAKE_TIMEOUT, handshake)
                .await
                .map_err(|_| std::io::Error::other("worker handshake timed out"))??;
            relay_through_worker(client, worker, resp, target_host, target_port).await
        }
        #[cfg(unix)]
        WorkerSocksAddr::Unix(path) => {
            let path = path.clone();
            let handshake = async move {
                let mut worker = tokio::net::UnixStream::connect(&path).await?;
                worker.write_all(&[VER, 1, AUTH_NONE]).await?;
                let mut resp = [0u8; 2];
                worker.read_exact(&mut resp).await?;
                Ok::<_, std::io::Error>((worker, resp))
            };
            let (worker, resp) = tokio::time::timeout(WORKER_HANDSHAKE_TIMEOUT, handshake)
                .await
                .map_err(|_| std::io::Error::other("worker handshake timed out"))??;
            relay_through_worker(client, worker, resp, target_host, target_port).await
        }
        // Box mode (and therefore `WorkerSocksAddr::Unix`) only ever runs on
        // the fleet's Linux boxes. This arm exists so the crate still
        // compiles on the Windows dev/test target — it is never reached
        // there, since nothing on Windows constructs a box-mode config.
        #[cfg(not(unix))]
        WorkerSocksAddr::Unix(_path) => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unix socket worker targets are not supported on this platform",
        )),
    }
}

/// Shared tail of [`forward_via_worker`]: once a worker stream is connected
/// and its SOCKS5 greeting reply (`resp`) is in hand, the rest of the
/// handshake (CONNECT request, reply, relay) is identical whether `worker`
/// is a `TcpStream` or a `UnixStream` — the worker speaks the same SOCKS5
/// protocol over either transport.
async fn relay_through_worker<S, W>(
    client: &mut S,
    mut worker: W,
    resp: [u8; 2],
    target_host: &str,
    target_port: u16,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    W: AsyncRead + AsyncWrite + Unpin,
{
    if resp[0] != VER || resp[1] != AUTH_NONE {
        return Err(std::io::Error::other("worker: auth negotiation failed"));
    }

    // CONNECT request with ATYP=domain.
    let host_bytes = target_host.as_bytes();
    if host_bytes.len() > 255 {
        return Err(std::io::Error::other("target hostname too long"));
    }
    let mut req = Vec::with_capacity(7 + host_bytes.len());
    req.extend_from_slice(&[VER, CMD_CONNECT, 0x00, ATYP_DOMAIN, host_bytes.len() as u8]);
    req.extend_from_slice(host_bytes);
    req.extend_from_slice(&target_port.to_be_bytes());
    worker.write_all(&req).await?;

    // Read worker's SOCKS5 reply (10 bytes for IPv4 bind addr) — bounded so
    // a stalled worker chain doesn't pin the router on a dead sibling.
    let mut reply_hdr = [0u8; 4];
    tokio::time::timeout(
        WORKER_HANDSHAKE_TIMEOUT,
        worker.read_exact(&mut reply_hdr),
    )
    .await
    .map_err(|_| std::io::Error::other("worker CONNECT reply timed out"))??;
    if reply_hdr[0] != VER || reply_hdr[1] != REP_SUCCESS {
        return Err(std::io::Error::other(format!(
            "worker CONNECT failed: rep=0x{:02x}",
            reply_hdr[1]
        )));
    }
    // Consume bind addr/port based on atyp.
    let bind_atyp = reply_hdr[3];
    match bind_atyp {
        ATYP_IPV4 => {
            let mut rest = [0u8; 6]; // 4 addr + 2 port
            worker.read_exact(&mut rest).await?;
        }
        ATYP_IPV6 => {
            let mut rest = [0u8; 18]; // 16 addr + 2 port
            worker.read_exact(&mut rest).await?;
        }
        ATYP_DOMAIN => {
            let mut len_buf = [0u8; 1];
            worker.read_exact(&mut len_buf).await?;
            let len = len_buf[0] as usize;
            let mut rest = vec![0u8; len + 2];
            worker.read_exact(&mut rest).await?;
        }
        _ => {
            return Err(std::io::Error::other(format!(
                "worker returned unknown bind atyp: 0x{bind_atyp:02x}"
            )));
        }
    }

    // Tell original client: success.
    write_socks5_success(client).await;

    // Bidirectional relay: client ↔ worker.
    tokio::io::copy_bidirectional(client, &mut worker)
        .await
        .map(|_| ())
        .map_err(std::io::Error::from)
}

async fn write_socks5_success<S: AsyncWrite + Unpin>(stream: &mut S) {
    let _ = stream
        .write_all(&[VER, REP_SUCCESS, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await;
}

async fn write_socks5_error<S: AsyncWrite + Unpin>(stream: &mut S, rep: u8) {
    let _ = stream
        .write_all(&[VER, rep, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_local_channel_maps_edge_client_entrance() {
        assert_eq!(
            resolve_local_channel("edge-client-entrance"),
            Some(LocalChannel::EdgeClientEntrance)
        );
        // The resolved channel's own label round-trips — the config string
        // IS the channel's stable name, not a parallel vocabulary.
        assert_eq!(LocalChannel::EdgeClientEntrance.label(), "edge-client-entrance");
    }

    #[test]
    fn resolve_local_channel_rejects_unknown_names() {
        assert_eq!(resolve_local_channel(""), None);
        assert_eq!(resolve_local_channel("node-chain"), None);
        assert_eq!(resolve_local_channel("edge-client-entrance "), None);
    }

    /// A unix-socket round trip through `handle_router_connection` — the same
    /// per-connection function the box-mode local-channel listener drives.
    ///
    /// Binds a REAL unix socket under the OS temp dir rather than through
    /// `mirage_core::local_endpoint::LocalChannel::bind()`: that helper binds
    /// the hardcoded fleet path `/run/mirage/...` and applies a uid grant via
    /// `getpwnam_r`, both privileged operations this crate does not own and
    /// must not re-test here (mirage2web is read-only for this task; its own
    /// bind/grant contract has its own tests). What this test proves is OURS:
    /// that a SOCKS5 CONNECT arriving over a `UnixStream` — the shape the
    /// node's entrance actually sends once `LocalChannel::bind()` hands the
    /// listener a connection — is parsed and relayed exactly like the TCP
    /// listener's, ending at a worker's SOCKS5 port and echoing bytes back.
    #[cfg(unix)]
    #[tokio::test]
    async fn socks5_connect_over_unix_stream_reaches_router() {
        use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
        use tokio::net::{UnixListener, UnixStream};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let sock_path = std::env::temp_dir()
            .join(format!("cli2-router-test-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);

        // Mock worker: a bare TCP SOCKS5 upstream that accepts one CONNECT
        // and then echoes bytes — proves the relay is bidirectional, not just
        // that the reply header was forwarded.
        let worker_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock worker");
        let worker_addr = worker_listener.local_addr().expect("worker addr");
        tokio::spawn(async move {
            let (mut w, _) = worker_listener.accept().await.expect("worker accept");
            let mut greeting = [0u8; 3];
            w.read_exact(&mut greeting).await.expect("worker read greeting");
            w.write_all(&[VER, AUTH_NONE]).await.expect("worker auth reply");
            let mut req_hdr = [0u8; 5];
            w.read_exact(&mut req_hdr).await.expect("worker read req hdr");
            let host_len = req_hdr[4] as usize;
            let mut rest = vec![0u8; host_len + 2];
            w.read_exact(&mut rest).await.expect("worker read host+port");
            w.write_all(&[VER, REP_SUCCESS, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await
                .expect("worker connect reply");
            // Echo whatever the relay sends until EOF.
            let mut buf = [0u8; 256];
            loop {
                match w.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if w.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let router = SupervisorRouter::new(vec![RouterChain {
            chain_id: "test-chain".to_string(),
            socks5_addr: WorkerSocksAddr::Tcp(worker_addr),
            policy: SocksFailurePolicy::Default,
            nature: WireNature::Direct,
        }]);

        let unix_listener = UnixListener::bind(&sock_path).expect("bind unix test socket");
        let accept_task = tokio::spawn(async move {
            let (stream, _addr) = unix_listener.accept().await.expect("unix accept");
            handle_router_connection(stream, &router).await
        });

        let mut client = UnixStream::connect(&sock_path)
            .await
            .expect("client connect to unix socket");

        // SOCKS5 greeting: ver=5, 1 method, no-auth.
        client.write_all(&[VER, 1, AUTH_NONE]).await.expect("write greeting");
        let mut greeting_reply = [0u8; 2];
        client.read_exact(&mut greeting_reply).await.expect("read greeting reply");
        assert_eq!(greeting_reply, [VER, AUTH_NONE]);

        // CONNECT request: domain "example.com":80.
        let host = b"example.com";
        let mut req = vec![VER, CMD_CONNECT, 0x00, ATYP_DOMAIN, host.len() as u8];
        req.extend_from_slice(host);
        req.extend_from_slice(&80u16.to_be_bytes());
        client.write_all(&req).await.expect("write connect request");

        let mut connect_reply = [0u8; 10];
        client.read_exact(&mut connect_reply).await.expect("read connect reply");
        assert_eq!(connect_reply[..2], [VER, REP_SUCCESS]);

        // Data plane: send bytes, expect them echoed back through the worker.
        client.write_all(b"ping").await.expect("write payload");
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.expect("read echoed payload");
        assert_eq!(&echoed, b"ping");

        drop(client);
        let _ = accept_task.await;
        let _ = std::fs::remove_file(&sock_path);
    }

    /// Box mode's other half: the ROUTER dials a `WorkerSocksAddr::Unix`
    /// target (the worker's own SOCKS5 listener, box-mode-bound per
    /// `mirage_client_core::socks5::bind_unix_socks5_listener`) instead of a
    /// TCP one. The client-facing side is plain TCP here — the previous test
    /// already proves the entrance-side unix transport; this one isolates
    /// the OTHER leg `forward_via_worker`'s `WorkerSocksAddr::Unix` branch.
    #[cfg(unix)]
    #[tokio::test]
    async fn router_dials_a_unix_worker_target() {
        use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
        use tokio::net::UnixListener;

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let worker_sock_path = std::env::temp_dir()
            .join(format!("cli2-router-worker-test-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&worker_sock_path);

        // Mock worker: SOCKS5 over a UNIX socket, exactly what a box-mode
        // worker binds via `bind_unix_socks5_listener`.
        let worker_listener = UnixListener::bind(&worker_sock_path).expect("bind mock unix worker");
        let worker_path_for_task = worker_sock_path.clone();
        let worker_task = tokio::spawn(async move {
            let (mut w, _addr) = worker_listener.accept().await.expect("worker accept");
            let mut greeting = [0u8; 3];
            w.read_exact(&mut greeting).await.expect("worker read greeting");
            w.write_all(&[VER, AUTH_NONE]).await.expect("worker auth reply");
            let mut req_hdr = [0u8; 5];
            w.read_exact(&mut req_hdr).await.expect("worker read req hdr");
            let host_len = req_hdr[4] as usize;
            let mut rest = vec![0u8; host_len + 2];
            w.read_exact(&mut rest).await.expect("worker read host+port");
            w.write_all(&[VER, REP_SUCCESS, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await
                .expect("worker connect reply");
            let mut buf = [0u8; 256];
            loop {
                match w.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if w.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = std::fs::remove_file(&worker_path_for_task);
        });

        let router = SupervisorRouter::new(vec![RouterChain {
            chain_id: "edge-exit-1".to_string(),
            socks5_addr: WorkerSocksAddr::Unix(worker_sock_path.clone()),
            policy: SocksFailurePolicy::Default,
            nature: WireNature::Direct,
        }]);

        // Client-facing side: plain TCP loopback pair (a raw pair is enough —
        // `handle_router_connection` is generic over the client stream, and
        // that genericity is proven by the sibling unix-entrance test).
        let entrance = TcpListener::bind("127.0.0.1:0").await.expect("bind entrance");
        let entrance_addr = entrance.local_addr().expect("entrance addr");
        let accept_task = tokio::spawn(async move {
            let (stream, _peer) = entrance.accept().await.expect("entrance accept");
            handle_router_connection(stream, &router).await
        });

        let mut client = TcpStream::connect(entrance_addr).await.expect("client connect");
        client.write_all(&[VER, 1, AUTH_NONE]).await.expect("write greeting");
        let mut greeting_reply = [0u8; 2];
        client.read_exact(&mut greeting_reply).await.expect("read greeting reply");
        assert_eq!(greeting_reply, [VER, AUTH_NONE]);

        let host = b"example.com";
        let mut req = vec![VER, CMD_CONNECT, 0x00, ATYP_DOMAIN, host.len() as u8];
        req.extend_from_slice(host);
        req.extend_from_slice(&80u16.to_be_bytes());
        client.write_all(&req).await.expect("write connect request");

        let mut connect_reply = [0u8; 10];
        client.read_exact(&mut connect_reply).await.expect("read connect reply");
        assert_eq!(connect_reply[..2], [VER, REP_SUCCESS], "router must have dialled the unix worker target successfully");

        client.write_all(b"ping").await.expect("write payload");
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.expect("read echoed payload");
        assert_eq!(&echoed, b"ping", "bytes must relay through the unix-dialled worker and back");

        drop(client);
        let _ = accept_task.await;
        let _ = worker_task.await;
        let _ = std::fs::remove_file(&worker_sock_path);
    }
}
