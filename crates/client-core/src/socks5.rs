//! Minimal SOCKS5 server (RFC 1928).
//!
//! Supports CONNECT (0x01) and UDP ASSOCIATE (0x03) with no authentication.
//! Handles both IPv4 (0x01), domain (0x03), and IPv6 (0x04) addresses.
//!
//! ## Multi-chain failover
//!
//! Each [`Socks5Server`] is bound to one chain's [`TunnelPool`] as its
//! primary pool.  An optional `failover_pools` list (sorted by priority)
//! allows the handler to try other chains when the primary pool is
//! exhausted.  This is the cross-chain failover mechanism described in the
//! multi-chain design.

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use mirage_core::{config::ClientConfig, error::MirageError, routing::RouteDecision, routing::RoutingTable};
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
pub struct Socks5Server {
    bind_addr: SocketAddr,
    /// `(chain_id, pool)` pairs used for failover, highest-priority first.
    /// Does not include the primary pool.
    failover_pools: Vec<(String, Arc<TunnelPool>)>,
}

impl Socks5Server {
    /// Create a new server that will listen on `bind_addr` with no failover.
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self {
            bind_addr,
            failover_pools: Vec::new(),
        }
    }

    /// Create a server that will listen on `bind_addr` with cross-chain
    /// failover pools (sorted highest-priority first, primary excluded).
    pub fn new_with_failover(
        bind_addr: SocketAddr,
        failover_pools: Vec<(String, Arc<TunnelPool>)>,
    ) -> Self {
        Self { bind_addr, failover_pools }
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

        loop {
            let (stream, peer) = listener.accept().await?;
            debug!(%peer, "new SOCKS5 connection");

            let pool = Arc::clone(&pool);
            let mux_pool = mux_pool.clone();
            let router = Arc::clone(&router);
            let failover = Arc::clone(&failover);

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
                            handle_socks5_connection(stream, pool, mux_pool, router, failover).await
                        }
                        ConnectionKind::Transparent => {
                            handle_transparent_connection(stream, pool, mux_pool, router, failover).await
                        }
                    }
                };

                #[cfg(not(target_os = "linux"))]
                let result = handle_socks5_connection(stream, pool, mux_pool, router, failover).await;

                if let Err(e) = result {
                    error!(%peer, "connection error: {e}");
                }
            });
        }
    }
}

/// Lease a tunnel from the primary pool, falling over to other chains when
/// the primary is exhausted.
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
    pool: Arc<TunnelPool>,
    mux_pool: Option<Arc<MuxPool>>,
    router: Arc<RoutingTable>,
    failover: Arc<Vec<(String, Arc<TunnelPool>)>>,
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
            let dest = match atyp {
                SOCKS5_ATYP_IPV4 => {
                    let mut addr_buf = [0u8; 4];
                    stream.read_exact(&mut addr_buf).await?;
                    let mut port_buf = [0u8; 2];
                    stream.read_exact(&mut port_buf).await?;
                    let port = u16::from_be_bytes(port_buf);
                    let ip = std::net::Ipv4Addr::from(addr_buf);
                    Destination::Ip(SocketAddr::from((ip, port)))
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
                    let port = u16::from_be_bytes(port_buf);
                    Destination::Domain(domain, port)
                }
                SOCKS5_ATYP_IPV6 => {
                    let mut addr_buf = [0u8; 16];
                    stream.read_exact(&mut addr_buf).await?;
                    let mut port_buf = [0u8; 2];
                    stream.read_exact(&mut port_buf).await?;
                    let port = u16::from_be_bytes(port_buf);
                    let ip = std::net::Ipv6Addr::from(addr_buf);
                    Destination::Ip(SocketAddr::from((ip, port)))
                }
                _ => {
                    send_socks5_error(&mut stream, SOCKS5_REP_ATYP_NOT_SUPPORTED).await;
                    return Err(MirageError::Protocol(format!(
                        "SOCKS5 unsupported atyp: 0x{atyp:02x}"
                    )));
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
                RouteDecision::Direct => route_direct(&mut stream, &target_addr).await,
                RouteDecision::Chain => {
                    if let Some(mp) = mux_pool {
                        route_via_mux_pool(&mut stream, mp, &target_addr).await
                    } else {
                        route_via_pool_with_failover(&mut stream, pool, &failover, &target_addr).await
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

/// Lease a pre-warmed tunnel from the pool (with failover), send the target
/// address, then relay.
async fn route_via_pool_with_failover(
    client: &mut TcpStream,
    pool: Arc<TunnelPool>,
    failover: &[(String, Arc<TunnelPool>)],
    target_addr: &str,
) -> Result<(), MirageError> {
    let mut lease = lease_with_failover(&pool, failover).await?;

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

    send_socks5_success(client).await?;

    lease.set_relay_active(true);
    let tunnel = lease.take_tunnel().ok_or(MirageError::TunnelClosed)?;
    let mut compat = TunnelCompat::wrap(tunnel);
    let result = relay_loop(client, &mut compat).await;
    lease.poison();
    result
}

/// Open a mux stream, send the target address, then relay.
async fn route_via_mux_pool(
    client: &mut TcpStream,
    mux_pool: Arc<MuxPool>,
    target_addr: &str,
) -> Result<(), MirageError> {
    let is_onion = mux_pool.is_onion.load(std::sync::atomic::Ordering::Acquire);
    let mut stream = match mux_pool.open_stream().await {
        Ok(s) => s,
        Err(MirageError::CircuitOpen) => {
            debug!(target = %target_addr, "mux circuit open: fast-rejecting");
            send_socks5_error(client, 0x04).await; // host unreachable
            return Err(MirageError::CircuitOpen);
        }
        Err(e) => return Err(e),
    };

    if is_onion {
        let target_bytes = target_addr.as_bytes();
        let len_prefix = (target_bytes.len() as u16).to_be_bytes();
        let mut framed = Vec::with_capacity(2 + target_bytes.len());
        framed.extend_from_slice(&len_prefix);
        framed.extend_from_slice(target_bytes);
        tokio::io::AsyncWriteExt::write_all(&mut stream, &framed).await
            .map_err(MirageError::Io)?;
    } else {
        tokio::io::AsyncWriteExt::write_all(&mut stream, target_addr.as_bytes()).await
            .map_err(MirageError::Io)?;
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
        RouteDecision::Chain => {
            if let Some(mp) = mux_pool {
                route_via_mux_pool(&mut stream, mp, &target_addr).await?;
            } else {
                let mut lease = lease_with_failover(&pool, &failover).await?;
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
async fn send_socks5_udp_success(
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
