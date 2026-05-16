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

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
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

/// One chain known to the supervisor, with its worker's SOCKS5 address.
#[derive(Debug, Clone)]
pub struct RouterChain {
    pub chain_id: String,
    pub socks5_addr: SocketAddr,
}

// ── SupervisorRouter ─────────────────────────────────────────────────────────

/// Lightweight round-robin router over a static chain list.
///
/// Holds an ordered snapshot of `(chain_id, socks5_addr)` pairs.  On each
/// `pick()` call it returns chains ordered starting from the current cursor
/// position (wrapping around). The caller iterates in order, trying each
/// worker's SOCKS5 until one succeeds.
pub struct SupervisorRouter {
    chains: Vec<RouterChain>,
    cursor: AtomicU64,
}

impl SupervisorRouter {
    /// Build from config chains list.
    pub fn new(chains: Vec<RouterChain>) -> Arc<Self> {
        Arc::new(Self {
            chains,
            cursor: AtomicU64::new(0),
        })
    }

    /// Return ordered chain list starting at the current round-robin cursor.
    ///
    /// Returns an empty vec when no chains are configured.
    pub fn pick(&self) -> Vec<&RouterChain> {
        if self.chains.is_empty() {
            return Vec::new();
        }
        let n = self.chains.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) as usize % n;
        let mut result = Vec::with_capacity(n);
        for i in 0..n {
            result.push(&self.chains[(start + i) % n]);
        }
        result
    }
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

// ── Connection handler ────────────────────────────────────────────────────────

async fn handle_router_connection(
    mut client: TcpStream,
    router: &SupervisorRouter,
) -> std::io::Result<()> {
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

    for chain in &candidates {
        debug!(chain = %chain.chain_id, worker = %chain.socks5_addr, "router: trying worker");
        match forward_via_worker(&mut client, chain.socks5_addr, &target_host, target_port).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!(chain = %chain.chain_id, error = %e, "router: worker failed — trying next");
            }
        }
    }

    write_socks5_error(&mut client, REP_GENERAL).await;
    Err(std::io::Error::other("all workers exhausted"))
}

/// Read the SOCKS5 target address + port from the stream.
async fn read_socks5_target(
    stream: &mut TcpStream,
    atyp: u8,
) -> std::io::Result<(String, u16)> {
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
async fn forward_via_worker(
    client: &mut TcpStream,
    worker_addr: SocketAddr,
    target_host: &str,
    target_port: u16,
) -> std::io::Result<()> {
    let handshake = async {
        let mut worker = tokio::net::TcpStream::connect(worker_addr).await?;
        worker.write_all(&[VER, 1, AUTH_NONE]).await?;
        let mut resp = [0u8; 2];
        worker.read_exact(&mut resp).await?;
        Ok::<_, std::io::Error>((worker, resp))
    };
    let (mut worker, resp) =
        tokio::time::timeout(WORKER_HANDSHAKE_TIMEOUT, handshake)
            .await
            .map_err(|_| std::io::Error::other("worker handshake timed out"))??;
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

async fn write_socks5_success(stream: &mut TcpStream) {
    let _ = stream
        .write_all(&[VER, REP_SUCCESS, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await;
}

async fn write_socks5_error(stream: &mut TcpStream, rep: u8) {
    let _ = stream
        .write_all(&[VER, rep, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await;
}
