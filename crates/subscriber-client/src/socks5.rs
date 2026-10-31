//! A minimal, purpose-built SOCKS5 (RFC 1928) listener over one
//! [`EdgeSessionSupervisor`] — the customer's own local surface.
//!
//! **CONNECT** parses the request's destination (IPv4 / IPv6 / domain,
//! `ATYP` 0x01/0x04/0x03) into the literal `host:port` string
//! [`EdgeSession::open_tcp_stream`] carries as a [`StreamOpen`] target — a
//! domain name is passed through UNRESOLVED, so DNS resolves at the exit,
//! never on this machine.
//!
//! **UDP ASSOCIATE** (§7) is served for real by
//! [`crate::udp_associate::serve_udp_association`] — this module owns only
//! the SOCKS5 wire-level pieces that path reuses: the per-datagram header
//! parser ([`parse_socks5_udp_request`]) and the ASSOCIATE reply writer
//! ([`write_udp_associate_reply`]), both `pub(crate)` for that.
//!
//! **RAW IP** (command 0x05) is the mono-protocol data path: the device
//! asks for the whole IP packets of the tunnel, and they ride the QUIC
//! datagrams of the ONE connection the edge session rides. The wire is the
//! SOCKS5 request `05 05 00 03 09 "mirage-ip" 00 00` (version 5, command
//! 5, no auth, domain `mirage-ip`, port 0) and then frames in BOTH
//! directions: `[2B big-endian length][IP packet]`. The server answers with
//! the ordinary ten-byte success reply, and the frames follow it.
//!
//! Not reused from `mirage_client_core::socks5::Socks5Server` on purpose:
//! that type is built around a `Router`/`ChainSet`/`TunnelPool` — a fixed
//! set of pre-built chains — which this crate no longer has. Every flow
//! here opens a fresh mux stream on the ONE live [`EdgeSession`], dynamically
//! at CONNECT/ASSOCIATE time, never resolved from a pre-built chain table.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mirage_protocol::plane::PacketPlane;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::scope::EgressScope;
use crate::session::EdgeSessionSupervisor;

const SOCKS5_VERSION: u8 = 0x05;
const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_NO_ACCEPTABLE: u8 = 0xFF;

const CMD_CONNECT: u8 = 0x01;
const CMD_UDP_ASSOCIATE: u8 = 0x03;
/// The mono-protocol command: the whole IP packets of the tunnel, in and
/// out. Not a standard SOCKS5 command - it is this client's own.
const CMD_RAW_IP: u8 = 0x05;

const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

pub(crate) const REP_SUCCESS: u8 = 0x00;
pub(crate) const REP_GENERAL_FAILURE: u8 = 0x01;
/// RFC 1928: connection not allowed by ruleset. Used when the destination
/// is outside the services the user selected.
pub(crate) const REP_NOT_ALLOWED: u8 = 0x02;
const REP_COMMAND_NOT_SUPPORTED: u8 = 0x07;
const REP_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;

/// How long a CONNECT waits for a live edge session before giving up and
/// replying general failure.
const CONNECT_SESSION_WAIT: Duration = Duration::from_secs(5);

/// How often the counters of the raw-IP path are logged.
const STATS_INTERVAL: Duration = Duration::from_secs(10);

/// The smallest an IPv4 header can be: twenty bytes.
const IPV4_MIN_HEADER: usize = 20;

/// The smallest an IPv6 header can be: forty bytes.
const IPV6_MIN_HEADER: usize = 40;

/// One raw-IP consumer at a time.
///
/// The tunnel device is the one consumer of the packet plane: a second one
/// would steal the packets from the first. The device that loses the race
/// is told "general failure" and closes.
static RAW_IP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The counters of the raw-IP path.
///
/// A packet that is dropped where nobody looks is a packet the operator
/// cannot account for: every drop point of this path has a number here.
#[derive(Debug)]
struct RawIpStats {
    /// The packets dropped because they do not fit the plane.
    dropped_too_large: AtomicU64,
    /// The packets that could not be sent.
    send_errors: AtomicU64,
    /// The frames that were not IP packets at all.
    malformed_frames: AtomicU64,
    /// The packets that went onto the plane.
    packets_to_plane: AtomicU64,
    /// The packets that came off the plane.
    packets_from_plane: AtomicU64,
}

impl RawIpStats {
    /// A fresh set of counters.
    const fn new() -> Self {
        Self {
            dropped_too_large: AtomicU64::new(0),
            send_errors: AtomicU64::new(0),
            malformed_frames: AtomicU64::new(0),
            packets_to_plane: AtomicU64::new(0),
            packets_from_plane: AtomicU64::new(0),
        }
    }

    /// The counters as plain numbers.
    fn snapshot(&self) -> RawIpSnapshot {
        RawIpSnapshot {
            dropped_too_large: self.dropped_too_large.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
            malformed_frames: self.malformed_frames.load(Ordering::Relaxed),
            packets_to_plane: self.packets_to_plane.load(Ordering::Relaxed),
            packets_from_plane: self.packets_from_plane.load(Ordering::Relaxed),
        }
    }
}

/// The counters of the raw-IP path as plain numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RawIpSnapshot {
    /// The packets dropped because they do not fit the plane.
    pub(crate) dropped_too_large: u64,
    /// The packets that could not be sent.
    pub(crate) send_errors: u64,
    /// The frames that were not IP packets at all.
    pub(crate) malformed_frames: u64,
    /// The packets that went onto the plane.
    pub(crate) packets_to_plane: u64,
    /// The packets that came off the plane.
    pub(crate) packets_from_plane: u64,
}

/// The counters of the raw-IP path of this process.
static RAW_IP_STATS: RawIpStats = RawIpStats::new();

/// The counters of the raw-IP path as plain numbers.
///
/// The snapshot is what the operator's log line and the tests read.
pub(crate) fn raw_ip_stats() -> RawIpSnapshot {
    RAW_IP_STATS.snapshot()
}

/// Every way reading a SOCKS5 request off the wire fails.
#[derive(Debug, Error)]
pub(crate) enum Socks5RequestError {
    #[error("socks5: client offered no acceptable auth method")]
    NoAcceptableAuthMethod,
    #[error("socks5: unsupported protocol version 0x{0:02x}")]
    UnsupportedVersion(u8),
    #[error("socks5: unsupported address type 0x{0:02x}")]
    UnsupportedAddressType(u8),
    #[error("socks5: destination address is malformed")]
    MalformedAddress,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Bind `bind` and serve SOCKS5 connections against `supervisor` until the
/// listener errors. Bind happens INSIDE this async fn (not before it is
/// spawned) — matches this crate's established pattern of standing up the
/// listener from within its own driving task.
pub async fn serve(
    bind: SocketAddr,
    supervisor: Arc<EdgeSessionSupervisor>,
    scope: Arc<EgressScope>,
) -> std::io::Result<()> {
    serve_slot(bind, crate::supervisor_slot::SupervisorSlot::new(supervisor), scope).await
}

/// Serve SOCKS5 against a swappable supervisor.
///
/// Every connection uses the supervisor that is live when the connection
/// arrives, so a new configuration takes effect over the same listener: the
/// listener, the process and the tunnel device all stay up.
pub async fn serve_slot(
    bind: SocketAddr,
    slot: Arc<crate::supervisor_slot::SupervisorSlot>,
    scope: Arc<EgressScope>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!(bind = %bind, "subscriber-client: SOCKS5 listener ready");
    loop {
        let (stream, peer) = listener.accept().await?;
        debug!(%peer, "socks5: accepted connection");
        let supervisor = slot.current();
        let scope = Arc::clone(&scope);
        tokio::spawn(async move {
            handle_connection(stream, supervisor, scope).await;
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    supervisor: Arc<EdgeSessionSupervisor>,
    scope: Arc<EgressScope>,
) {
    let (cmd, target) = match read_request(&mut stream).await {
        Ok(v) => v,
        Err(Socks5RequestError::UnsupportedAddressType(atyp)) => {
            let _ = write_reply(&mut stream, REP_ADDRESS_TYPE_NOT_SUPPORTED).await;
            debug!(atyp, "socks5: unsupported address type");
            return;
        }
        Err(e) => {
            debug!(error = %e, "socks5: request read failed");
            return;
        }
    };

    match cmd {
        CMD_CONNECT => handle_connect(stream, target, supervisor, scope).await,
        CMD_UDP_ASSOCIATE => {
            // A session that carries whole IP packets gives the client its
            // own UDP path inside the tunnel; an older session uses the mux
            // relay instead.
            let session = supervisor.wait_for_session(CONNECT_SESSION_WAIT).await;
            let stack = session.as_ref().and_then(|s| s.netstack());
            match stack {
                Some(stack) => {
                    if let Err(e) = crate::plane_udp::serve_udp_association(stream, stack).await {
                        debug!(error = %e, "socks5: udp associate over the plane ended with an io error");
                    }
                }
                None => {
                    if let Err(e) =
                        crate::udp_associate::serve_udp_association(stream, supervisor, scope).await
                    {
                        debug!(error = %e, "socks5: udp associate ended with an io error");
                    }
                }
            }
        }
        // The whole IP packets of the tunnel. The scope does not apply here:
        // the box is a plain L3 exit, and the device decides what goes into
        // the tunnel.
        CMD_RAW_IP => handle_raw_ip(stream, target, supervisor).await,
        other => {
            debug!(cmd = other, "socks5: unsupported command");
            let _ = write_reply(&mut stream, REP_COMMAND_NOT_SUPPORTED).await;
        }
    }
}

/// Serve the raw-IP command: the whole IP packets of the tunnel, in and out.
///
/// The device asks with the SOCKS5 request `05 05 00 03 09 "mirage-ip" 00
/// 00`; the answer is the ordinary success reply, and the frames follow it:
/// `[2B big-endian length][IP packet]` in both directions.
///
/// The packets ride the packet plane of the live edge session - the QUIC
/// datagrams of the same connection the edge hello rode.
async fn handle_raw_ip(
    mut stream: TcpStream,
    target: String,
    supervisor: Arc<EdgeSessionSupervisor>,
) {
    let session = match supervisor.wait_for_session(CONNECT_SESSION_WAIT).await {
        Some(session) => session,
        None => {
            warn!(target = %target, "socks5: no live edge session - refusing the raw-IP command");
            let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
            return;
        }
    };
    let Some(plane) = session.packet_plane() else {
        warn!(
            target = %target,
            "socks5: the live session carries no packet plane - refusing the raw-IP command"
        );
        let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
        return;
    };
    if session.netstack().is_some() {
        // The userspace stack owns this session's packets: one owner per
        // plane, or the two would steal the packets from each other.
        warn!(
            target = %target,
            "socks5: the userspace stack owns this session's packets - refusing the raw-IP command"
        );
        let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
        return;
    }
    // One device, one consumer: a second raw-IP connection is refused.
    if RAW_IP_ACTIVE.swap(true, Ordering::SeqCst) {
        warn!("socks5: a raw-IP consumer is already running - refusing the second one");
        let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
        return;
    }
    if write_reply(&mut stream, REP_SUCCESS).await.is_err() {
        RAW_IP_ACTIVE.store(false, Ordering::SeqCst);
        return;
    }
    info!(target = %target, "socks5: raw-IP consumer started");
    let outcome = raw_ip_pump(stream, plane).await;
    RAW_IP_ACTIVE.store(false, Ordering::SeqCst);
    match outcome {
        Ok(()) => info!("socks5: raw-IP consumer ended"),
        Err(e) => warn!(error = %e, "socks5: raw-IP consumer ended with an error"),
    }
}

/// Pump the raw-IP frames in both directions until one side ends.
async fn raw_ip_pump(stream: TcpStream, plane: Arc<dyn PacketPlane>) -> std::io::Result<()> {
    let (reader, writer) = tokio::io::split(stream);
    let upstream = raw_ip_upstream(reader, Arc::clone(&plane));
    let downstream = raw_ip_downstream(plane, writer);
    // The counters of the path, one line every ten seconds: a drop that
    // nobody sees is a drop nobody can account for.
    let stats = tokio::spawn(log_raw_ip_stats());
    let outcome = tokio::select! {
        result = upstream => result,
        result = downstream => result,
    };
    stats.abort();
    outcome
}

/// Log the counters of the raw-IP path, one line every ten seconds.
async fn log_raw_ip_stats() {
    let mut ticker = tokio::time::interval(STATS_INTERVAL);
    // The first tick of an interval fires at once: the line before it would
    // say nothing but zeroes.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let stats = raw_ip_stats();
        info!(
            to_plane = stats.packets_to_plane,
            from_plane = stats.packets_from_plane,
            dropped_too_large = stats.dropped_too_large,
            send_errors = stats.send_errors,
            malformed_frames = stats.malformed_frames,
            "socks5: raw-IP stats"
        );
    }
}

/// The device -> plane direction: read a frame, prepare it for the plane, and
/// put every piece on the wire.
async fn raw_ip_upstream(
    mut reader: tokio::io::ReadHalf<TcpStream>,
    plane: Arc<dyn PacketPlane>,
) -> std::io::Result<()> {
    let budget = plane.max_packet_size();
    let mut len_buf = [0u8; 2];
    loop {
        if let Err(e) = reader.read_exact(&mut len_buf).await {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                // The device closed the connection.
                return Ok(());
            }
            return Err(e);
        }
        let len = u16::from_be_bytes(len_buf) as usize;
        let mut packet = vec![0u8; len];
        if let Err(e) = reader.read_exact(&mut packet).await {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(());
            }
            return Err(e);
        }
        // The plane's own preparation says what may ride it: the pieces of
        // the packet, or nothing at all when it cannot ride.
        let pieces = crate::ipv4::prepare_for_plane(&packet, budget);
        if pieces.is_empty() {
            if is_malformed_frame(&packet) {
                RAW_IP_STATS.malformed_frames.fetch_add(1, Ordering::Relaxed);
                debug!(
                    len = packet.len(),
                    "socks5: dropping a frame that is not an IP packet"
                );
            } else {
                RAW_IP_STATS.dropped_too_large.fetch_add(1, Ordering::Relaxed);
                debug!(
                    len = packet.len(),
                    budget,
                    "socks5: dropping a packet that does not fit the plane"
                );
            }
            continue;
        }
        for piece in pieces {
            if piece.len() > budget {
                // The packet does not fit and cannot be split: the far side
                // would never reassemble it, so it is dropped here.
                RAW_IP_STATS.dropped_too_large.fetch_add(1, Ordering::Relaxed);
                debug!(
                    len = piece.len(),
                    budget,
                    "socks5: dropping a packet that does not fit the plane"
                );
                continue;
            }
            if let Err(e) = plane.send_packet(&piece).await {
                RAW_IP_STATS.send_errors.fetch_add(1, Ordering::Relaxed);
                warn!(error = %e, "socks5: the packet plane refused a packet");
                return Ok(());
            }
            RAW_IP_STATS.packets_to_plane.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The plane -> device direction: read a packet and write it as a frame.
async fn raw_ip_downstream(
    plane: Arc<dyn PacketPlane>,
    mut writer: tokio::io::WriteHalf<TcpStream>,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; mirage_protocol::plane::MAX_PACKET_SIZE];
    loop {
        let read = match plane.recv_packet(&mut buf).await {
            Ok(read) => read,
            Err(e) => {
                warn!(error = %e, "socks5: the packet plane ended");
                return Ok(());
            }
        };
        if read == 0 {
            continue;
        }
        let mut frame = Vec::with_capacity(2 + read);
        frame.extend_from_slice(&(read as u16).to_be_bytes());
        frame.extend_from_slice(&buf[..read]);
        if let Err(e) = writer.write_all(&frame).await {
            RAW_IP_STATS.send_errors.fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, "socks5: the device did not take the packet");
            return Err(e);
        }
        if let Err(e) = writer.flush().await {
            RAW_IP_STATS.send_errors.fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, "socks5: the device write could not be flushed");
            return Err(e);
        }
        RAW_IP_STATS.packets_from_plane.fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether the frame is not an IP packet at all, or is too short to carry
/// the header of its version.
///
/// The plane's own preparation says only "this cannot ride"; this says why:
/// a frame that is not IP is malformed, a frame that is IP but too big is
/// too large. The two are different failures, and the operator tells them
/// apart by the counter.
fn is_malformed_frame(packet: &[u8]) -> bool {
    match packet.first() {
        None => true,
        Some(&first) => match first >> 4 {
            4 => packet.len() < IPV4_MIN_HEADER,
            6 => packet.len() < IPV6_MIN_HEADER,
            _ => true,
        },
    }
}

async fn handle_connect(
    mut stream: TcpStream,
    target: String,
    supervisor: Arc<EdgeSessionSupervisor>,
    scope: Arc<EgressScope>,
) {
    let exe = crate::peer::owning_exe(&stream);
    if !scope.allows_flow(&target, exe.as_deref()) {
        debug!(target = %target, "socks5: destination outside the selected sites or apps");
        let _ = write_reply(&mut stream, REP_NOT_ALLOWED).await;
        return;
    }
    let session = match supervisor.wait_for_session(CONNECT_SESSION_WAIT).await {
        Some(session) => session,
        None => {
            warn!(target = %target, "socks5: no live edge session — refusing CONNECT");
            let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
            return;
        }
    };

    // A session that carries whole IP packets is a whole IP link: the client
    // runs its own TCP/IP stack over it and opens the flow THERE. No mux,
    // no system TUN, no routes, no helper process - and the name is
    // resolved through the tunnel.
    if let Some(stack) = session.netstack() {
        connect_over_plane(stream, target, stack).await;
        return;
    }
    if session.has_plane() {
        // The tunnel device owns this session's packets: one owner per
        // plane, or the two would steal the packets from each other.
        warn!(
            target = %target,
            "socks5: the tunnel device owns this session's packets - refusing CONNECT"
        );
        let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
        return;
    }

    match session.open_tcp_stream(&target).await {
        Ok(mut edge_stream) => {
            if write_reply(&mut stream, REP_SUCCESS).await.is_err() {
                return;
            }
            // One line per flow, at info: the tunnel's own log says what
            // the device did, this says what the edge did with it. A flow
            // that dies silently is the one thing that cannot be debugged
            // from the outside.
            let started = std::time::Instant::now();
            info!(target = %target, "socks5: CONNECT flow opened");
            match crate::relay::copy_bidirectional_with_flow_idle_timeout(&mut stream, &mut edge_stream)
                .await
            {
                Ok((to_edge, from_edge)) => {
                    info!(
                        target = %target,
                        to_edge,
                        from_edge,
                        ms = started.elapsed().as_millis() as u64,
                        "socks5: CONNECT flow ended"
                    );
                }
                Err(e) => {
                    warn!(
                        target = %target,
                        error = %e,
                        kind = ?e.kind(),
                        ms = started.elapsed().as_millis() as u64,
                        "socks5: CONNECT flow ended with an error"
                    );
                }
            }
        }
        Err(e) => {
            warn!(target = %target, error = %e, "socks5: failed to open a mux stream to the edge");
            let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
        }
    }
}

/// Open a CONNECT flow over the packet plane.
///
/// The flow is a real TCP connection inside the tunnel: the client's own
/// userspace stack opens it, the name is resolved through the tunnel, and
/// the bytes are copied between the local socket and that flow.
async fn connect_over_plane(
    mut stream: TcpStream,
    target: String,
    stack: Arc<crate::plane_net::PlaneStack>,
) {
    let Some((host, port)) = split_target(&target) else {
        warn!(target = %target, "socks5: the destination is malformed — refusing CONNECT");
        let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
        return;
    };
    // The name is resolved THROUGH the tunnel: the host resolver is never
    // consulted, so the answer cannot leak outside the tunnel.
    let addresses = match stack.resolve(&host).await {
        Ok(addresses) => addresses,
        Err(e) => {
            warn!(target = %target, error = %e, "socks5: the name did not resolve");
            let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
            return;
        }
    };
    let mut opened = None;
    for ip in addresses {
        let addr = SocketAddr::new(ip, port);
        match stack.connect(addr).await {
            Ok(flow) => {
                opened = Some(flow);
                break;
            }
            Err(e) => {
                debug!(target = %target, addr = %addr, error = %e, "socks5: the flow did not open");
            }
        }
    }
    let Some(mut flow) = opened else {
        warn!(target = %target, "socks5: no address answered — refusing CONNECT");
        let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
        return;
    };
    if write_reply(&mut stream, REP_SUCCESS).await.is_err() {
        return;
    }
    let started = std::time::Instant::now();
    info!(target = %target, "socks5: CONNECT flow opened over the plane");
    match crate::relay::copy_bidirectional_with_flow_idle_timeout(&mut stream, &mut flow).await {
        Ok((to_remote, from_remote)) => {
            info!(
                target = %target,
                to_remote,
                from_remote,
                ms = started.elapsed().as_millis() as u64,
                "socks5: CONNECT flow over the plane ended"
            );
        }
        Err(e) => {
            warn!(
                target = %target,
                error = %e,
                kind = ?e.kind(),
                ms = started.elapsed().as_millis() as u64,
                "socks5: CONNECT flow over the plane ended with an error"
            );
        }
    }
}

/// Split a `host:port` target into its host and its port.
///
/// The IPv6 form carries the host in brackets: `[::1]:443`.
pub(crate) fn split_target(target: &str) -> Option<(String, u16)> {
    if let Some(rest) = target.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':')?;
        return Some((host.to_owned(), port.parse().ok()?));
    }
    let (host, port) = target.rsplit_once(':')?;
    Some((host.to_owned(), port.parse().ok()?))
}

/// Read the SOCKS5 greeting + request, replying to the greeting inline (a
/// malformed or unacceptable greeting has no request to read yet). Returns
/// `(cmd, target)` where `target` is the literal `host:port` string a
/// `StreamOpen` carries.
async fn read_request(stream: &mut TcpStream) -> Result<(u8, String), Socks5RequestError> {
    let mut greeting_hdr = [0u8; 2];
    stream.read_exact(&mut greeting_hdr).await?;
    if greeting_hdr[0] != SOCKS5_VERSION {
        return Err(Socks5RequestError::UnsupportedVersion(greeting_hdr[0]));
    }
    let nmethods = greeting_hdr[1] as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&METHOD_NO_AUTH) {
        stream.write_all(&[SOCKS5_VERSION, METHOD_NO_ACCEPTABLE]).await?;
        return Err(Socks5RequestError::NoAcceptableAuthMethod);
    }
    stream.write_all(&[SOCKS5_VERSION, METHOD_NO_AUTH]).await?;

    let mut req_hdr = [0u8; 4];
    stream.read_exact(&mut req_hdr).await?;
    let [ver, cmd, _rsv, atyp] = req_hdr;
    if ver != SOCKS5_VERSION {
        return Err(Socks5RequestError::UnsupportedVersion(ver));
    }

    let addr_bytes: Vec<u8> = match atyp {
        ATYP_IPV4 => {
            let mut b = [0u8; 4];
            stream.read_exact(&mut b).await?;
            b.to_vec()
        }
        ATYP_IPV6 => {
            let mut b = [0u8; 16];
            stream.read_exact(&mut b).await?;
            b.to_vec()
        }
        ATYP_DOMAIN => {
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let mut b = vec![0u8; len_buf[0] as usize];
            stream.read_exact(&mut b).await?;
            b
        }
        other => return Err(Socks5RequestError::UnsupportedAddressType(other)),
    };
    let mut port_buf = [0u8; 2];
    stream.read_exact(&mut port_buf).await?;
    let port = u16::from_be_bytes(port_buf);

    let target = target_from_socks5_address(atyp, &addr_bytes, port)?;
    Ok((cmd, target))
}

/// Parse a SOCKS5 request's already-read `ATYP` + address bytes + port into
/// the literal `host:port` string a `StreamOpen` target carries. A domain
/// name is passed through verbatim — see the module doc.
fn target_from_socks5_address(
    atyp: u8,
    addr_bytes: &[u8],
    port: u16,
) -> Result<String, Socks5RequestError> {
    match atyp {
        ATYP_IPV4 => {
            let octets: [u8; 4] =
                addr_bytes.try_into().map_err(|_| Socks5RequestError::MalformedAddress)?;
            Ok(format!("{}:{port}", Ipv4Addr::from(octets)))
        }
        ATYP_DOMAIN => {
            let domain =
                std::str::from_utf8(addr_bytes).map_err(|_| Socks5RequestError::MalformedAddress)?;
            Ok(format!("{domain}:{port}"))
        }
        ATYP_IPV6 => {
            let octets: [u8; 16] =
                addr_bytes.try_into().map_err(|_| Socks5RequestError::MalformedAddress)?;
            Ok(format!("[{}]:{port}", Ipv6Addr::from(octets)))
        }
        other => Err(Socks5RequestError::UnsupportedAddressType(other)),
    }
}

pub(crate) async fn write_reply(stream: &mut TcpStream, rep: u8) -> std::io::Result<()> {
    // BND.ADDR/BND.PORT are unused by every caller of this listener (it
    // never tells the client to reconnect elsewhere) — 0.0.0.0:0, matching
    // the crate's previous SOCKS5 wiring.
    stream
        .write_all(&[SOCKS5_VERSION, rep, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await
}

/// Reply to a UDP ASSOCIATE request with `rep` and the relay's own bound
/// `BND.ADDR`/`BND.PORT` — unlike CONNECT's [`write_reply`] (always
/// `0.0.0.0:0`, unused by every one of ITS callers), a UDP ASSOCIATE client
/// needs this address for real: RFC 1928 §7 requires every subsequent UDP
/// datagram be sent TO it.
pub(crate) async fn write_udp_associate_reply(
    stream: &mut TcpStream,
    rep: u8,
    bound: SocketAddr,
) -> std::io::Result<()> {
    let mut out = vec![SOCKS5_VERSION, rep, 0x00];
    match bound {
        SocketAddr::V4(v4) => {
            out.push(ATYP_IPV4);
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            out.push(ATYP_IPV6);
            out.extend_from_slice(&v6.ip().octets());
            out.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
    stream.write_all(&out).await
}

/// One already-received UDP datagram's SOCKS5 §7 header, parsed:
/// `[2B RSV][1B FRAG][1B ATYP][DST.ADDR][DST.PORT][DATA]`. Pure, no I/O —
/// used by `crate::udp_associate` against one `recv_from` buffer at a time.
#[derive(Debug)]
pub(crate) struct Socks5UdpRequest {
    /// `0` for an unfragmented datagram — `crate::udp_associate` refuses
    /// anything else (RFC 1928 leaves reassembly optional; this deployment
    /// implements none of it, see that module's doc).
    pub frag: u8,
    /// `ATYP + DST.ADDR + DST.PORT` exactly as received — echoed back
    /// verbatim in every reply belonging to this destination's flow, so a
    /// client that only understands the ATYP it itself sent (e.g.
    /// `mirage2tun`'s `parse_socks5_udp_response`, IPv4/IPv6 only) always
    /// gets back the same shape.
    pub header_echo: Vec<u8>,
    /// The `host:port` string this destination becomes as a `StreamOpen`
    /// target — see [`target_from_socks5_address`].
    pub target: String,
    pub data: Vec<u8>,
}

/// Every way parsing one UDP datagram's SOCKS5 header fails.
#[derive(Debug, Error)]
pub(crate) enum Socks5UdpHeaderError {
    #[error("socks5 udp: datagram truncated (need at least {need} bytes, have {have})")]
    Truncated { need: usize, have: usize },
    #[error("socks5 udp: malformed destination: {0}")]
    Address(#[from] Socks5RequestError),
}

pub(crate) fn parse_socks5_udp_request(buf: &[u8]) -> Result<Socks5UdpRequest, Socks5UdpHeaderError> {
    if buf.len() < 4 {
        return Err(Socks5UdpHeaderError::Truncated { need: 4, have: buf.len() });
    }
    let frag = buf[2];
    let atyp = buf[3];
    let mut pos = 4usize;
    let addr_bytes: &[u8] = match atyp {
        ATYP_IPV4 => {
            let end = pos + 4;
            if buf.len() < end {
                return Err(Socks5UdpHeaderError::Truncated { need: end, have: buf.len() });
            }
            let bytes = &buf[pos..end];
            pos = end;
            bytes
        }
        ATYP_IPV6 => {
            let end = pos + 16;
            if buf.len() < end {
                return Err(Socks5UdpHeaderError::Truncated { need: end, have: buf.len() });
            }
            let bytes = &buf[pos..end];
            pos = end;
            bytes
        }
        ATYP_DOMAIN => {
            if buf.len() < pos + 1 {
                return Err(Socks5UdpHeaderError::Truncated { need: pos + 1, have: buf.len() });
            }
            let len = buf[pos] as usize;
            pos += 1;
            let end = pos + len;
            if buf.len() < end {
                return Err(Socks5UdpHeaderError::Truncated { need: end, have: buf.len() });
            }
            let bytes = &buf[pos..end];
            pos = end;
            bytes
        }
        other => return Err(Socks5RequestError::UnsupportedAddressType(other).into()),
    };
    if buf.len() < pos + 2 {
        return Err(Socks5UdpHeaderError::Truncated { need: pos + 2, have: buf.len() });
    }
    let port = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
    let header_echo = buf[3..pos + 2].to_vec();
    pos += 2;
    let target = target_from_socks5_address(atyp, addr_bytes, port)?;
    Ok(Socks5UdpRequest { frag, header_echo, target, data: buf[pos..].to_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_control_plane::ExitPreference;

    // ── target_from_socks5_address: pure, no I/O ────────────────────────

    #[test]
    fn ipv4_address_becomes_dotted_host_port() {
        assert_eq!(
            target_from_socks5_address(ATYP_IPV4, &[93, 184, 216, 34], 443).expect("valid"),
            "93.184.216.34:443"
        );
    }

    #[test]
    fn domain_name_passes_through_unresolved() {
        assert_eq!(
            target_from_socks5_address(ATYP_DOMAIN, b"example.com", 443).expect("valid"),
            "example.com:443",
            "a domain must ride through verbatim — DNS resolves at the exit, never here"
        );
    }

    #[test]
    fn ipv6_address_is_bracketed() {
        let loopback = Ipv6Addr::LOCALHOST.octets();
        assert_eq!(
            target_from_socks5_address(ATYP_IPV6, &loopback, 443).expect("valid"),
            "[::1]:443"
        );
    }

    #[test]
    fn unknown_address_type_is_named() {
        let err = target_from_socks5_address(0x7f, &[], 0).expect_err("must refuse");
        assert!(matches!(err, Socks5RequestError::UnsupportedAddressType(0x7f)));
    }

    // ── parse_socks5_udp_request: pure, no I/O ──────────────────────────

    #[test]
    fn parse_udp_request_ipv4() {
        let mut buf = vec![0x00, 0x00, 0x00, ATYP_IPV4];
        buf.extend_from_slice(&[93, 184, 216, 34]);
        buf.extend_from_slice(&443u16.to_be_bytes());
        buf.extend_from_slice(b"payload");
        let req = parse_socks5_udp_request(&buf).expect("valid header");
        assert_eq!(req.frag, 0);
        assert_eq!(req.target, "93.184.216.34:443");
        assert_eq!(req.data, b"payload");
        assert_eq!(req.header_echo, buf[3..10], "ATYP+ADDR+PORT, verbatim");
    }

    #[test]
    fn parse_udp_request_ipv6() {
        let mut buf = vec![0x00, 0x00, 0x00, ATYP_IPV6];
        buf.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        buf.extend_from_slice(&53u16.to_be_bytes());
        buf.extend_from_slice(b"dns-query");
        let req = parse_socks5_udp_request(&buf).expect("valid header");
        assert_eq!(req.target, "[::1]:53");
        assert_eq!(req.data, b"dns-query");
    }

    #[test]
    fn parse_udp_request_domain() {
        let mut buf = vec![0x00, 0x00, 0x00, ATYP_DOMAIN, 11u8];
        buf.extend_from_slice(b"example.com");
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"data");
        let req = parse_socks5_udp_request(&buf).expect("valid header");
        assert_eq!(req.target, "example.com:80");
        assert_eq!(req.data, b"data");
    }

    #[test]
    fn parse_udp_request_preserves_frag_for_the_caller_to_refuse() {
        let mut buf = vec![0x00, 0x00, 0x07, ATYP_IPV4]; // FRAG = 7, not 0.
        buf.extend_from_slice(&[1, 2, 3, 4]);
        buf.extend_from_slice(&1u16.to_be_bytes());
        let req = parse_socks5_udp_request(&buf).expect("header still parses — refusal is the caller's job");
        assert_eq!(req.frag, 7);
    }

    #[test]
    fn parse_udp_request_truncated_is_named() {
        let err = parse_socks5_udp_request(&[0x00, 0x00]).expect_err("too short for even a header");
        assert!(matches!(err, Socks5UdpHeaderError::Truncated { need: 4, have: 2 }));
    }

    #[test]
    fn parse_udp_request_unsupported_address_type_is_named() {
        let buf = [0x00, 0x00, 0x00, 0x7f];
        let err = parse_socks5_udp_request(&buf).expect_err("must refuse");
        assert!(matches!(
            err,
            Socks5UdpHeaderError::Address(Socks5RequestError::UnsupportedAddressType(0x7f))
        ));
    }

    async fn free_local_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral");
        listener.local_addr().expect("local addr")
    }

    // --- the counters of the raw-IP path ---

    #[test]
    fn the_stats_snapshot_carries_the_counters() {
        let before = raw_ip_stats();
        RAW_IP_STATS.dropped_too_large.fetch_add(3, Ordering::Relaxed);
        RAW_IP_STATS.send_errors.fetch_add(1, Ordering::Relaxed);
        RAW_IP_STATS.malformed_frames.fetch_add(1, Ordering::Relaxed);
        RAW_IP_STATS.packets_to_plane.fetch_add(5, Ordering::Relaxed);
        RAW_IP_STATS.packets_from_plane.fetch_add(7, Ordering::Relaxed);
        let after = raw_ip_stats();

        assert_eq!(after.dropped_too_large, before.dropped_too_large + 3);
        assert_eq!(after.send_errors, before.send_errors + 1);
        assert_eq!(after.malformed_frames, before.malformed_frames + 1);
        assert_eq!(after.packets_to_plane, before.packets_to_plane + 5);
        assert_eq!(after.packets_from_plane, before.packets_from_plane + 7);
    }

    #[test]
    fn a_frame_that_is_not_ip_is_malformed() {
        assert!(is_malformed_frame(&[]));
        // A version nibble that is neither 4 nor 6.
        let mut not_ip = vec![0u8; 100];
        not_ip[0] = 0x50;
        assert!(is_malformed_frame(&not_ip));
        // Too short to carry the header of its own version.
        assert!(is_malformed_frame(&[0x45, 0, 0]));
        assert!(is_malformed_frame(&[0x60; 10]));
    }

    #[test]
    fn a_frame_that_is_ip_is_not_malformed() {
        let mut ipv4 = vec![0u8; IPV4_MIN_HEADER];
        ipv4[0] = 0x45;
        assert!(!is_malformed_frame(&ipv4));
        let mut ipv6 = vec![0u8; IPV6_MIN_HEADER];
        ipv6[0] = 0x60;
        assert!(!is_malformed_frame(&ipv6));
    }

    #[test]
    fn the_frames_the_plane_refuses_are_told_apart() {
        // An IPv6 packet that does not fit: the plane refuses it, and it is
        // too large -- not malformed.
        let mut ipv6 = vec![0u8; 2000];
        ipv6[0] = 0x60;
        assert!(crate::ipv4::prepare_for_plane(&ipv6, 1280).is_empty());
        assert!(!is_malformed_frame(&ipv6));

        // A frame that is not IP: the plane refuses it, and it is malformed.
        let mut not_ip = vec![0u8; 100];
        not_ip[0] = 0x50;
        assert!(crate::ipv4::prepare_for_plane(&not_ip, 1280).is_empty());
        assert!(is_malformed_frame(&not_ip));
    }

    #[tokio::test]
    async fn connect_with_no_live_session_replies_general_failure() {
        let bind = free_local_addr().await;
        let listener = TcpListener::bind(bind).await.expect("bind");
        // No entrances at all -> the supervisor never has a live session.
        let supervisor =
            EdgeSessionSupervisor::spawn(Vec::new(), b"MESH2".to_vec(), [0x11u8; 32], ExitPreference::Auto);
        tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.expect("accept");
            handle_connection(stream, supervisor, std::sync::Arc::new(EgressScope::all())).await;
        });

        let mut client = TcpStream::connect(bind).await.expect("connect");
        client.write_all(&[0x05, 0x01, 0x00]).await.expect("write greeting");
        let mut greeting_reply = [0u8; 2];
        client.read_exact(&mut greeting_reply).await.expect("read greeting reply");
        assert_eq!(greeting_reply, [0x05, 0x00]);

        // CMD=CONNECT(0x01), ATYP=domain, "example.com:443".
        let mut req = vec![0x05, 0x01, 0x00, 0x03, 11u8];
        req.extend_from_slice(b"example.com");
        req.extend_from_slice(&443u16.to_be_bytes());
        client.write_all(&req).await.expect("write request");

        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.expect("read reply");
        assert_eq!(reply[0], SOCKS5_VERSION);
        assert_eq!(reply[1], REP_GENERAL_FAILURE);
    }

    #[tokio::test]
    async fn raw_ip_with_no_live_session_replies_general_failure() {
        let bind = free_local_addr().await;
        let listener = TcpListener::bind(bind).await.expect("bind");
        // No entrances at all -> the supervisor never has a live session, and
        // a live session without a packet plane would not do either.
        let supervisor =
            EdgeSessionSupervisor::spawn(Vec::new(), b"MESH2".to_vec(), [0x11u8; 32], ExitPreference::Auto);
        tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.expect("accept");
            handle_connection(stream, supervisor, std::sync::Arc::new(EgressScope::all())).await;
        });

        let mut client = TcpStream::connect(bind).await.expect("connect");
        client.write_all(&[0x05, 0x01, 0x00]).await.expect("write greeting");
        let mut greeting_reply = [0u8; 2];
        client.read_exact(&mut greeting_reply).await.expect("read greeting reply");
        assert_eq!(greeting_reply, [0x05, 0x00]);

        // CMD=RAW_IP(0x05), ATYP=domain, "mirage-ip:0".
        let mut req = vec![0x05, CMD_RAW_IP, 0x00, 0x03, 9u8];
        req.extend_from_slice(b"mirage-ip");
        req.extend_from_slice(&0u16.to_be_bytes());
        client.write_all(&req).await.expect("write request");

        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.expect("read reply");
        assert_eq!(reply[0], SOCKS5_VERSION);
        assert_eq!(reply[1], REP_GENERAL_FAILURE);
    }
}
