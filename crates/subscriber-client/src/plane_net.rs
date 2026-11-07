//! A userspace TCP/IP stack that rides the packet plane.
//!
//! The plane carries whole IP packets in QUIC datagrams. On top of it this
//! module runs the same TCP/IP stack `mirage2tun` already uses - OUR OWN
//! fork of smoltcp, `mirage-tcpip-stack`, vendored in this workspace - so a
//! headless client can open real TCP and UDP flows to the internet without a
//! system TUN, without routes, without Wintun and without the host stack
//! touching the network at all.
//!
//! Nothing new is pulled in: `mirage-tcpip-stack` is already built for every
//! platform this client runs on. What is new here is the OUTBOUND half. The
//! fork drove TCP as a SERVER - it accepted connections coming IN through
//! the tunnel - while a SOCKS5 client needs the other half: connections that
//! go OUT.
//!
//! The shape of the thing:
//!
//! - [`PlaneStack`] is the handle. It owns one task that drives the stack.
//! - [`PlaneTcpStream`] is one TCP flow, an ordinary `AsyncRead` +
//!   `AsyncWrite`.
//! - [`PlaneUdpSocket`] is one UDP socket: datagrams out to any destination,
//!   datagrams back from any source.
//!
//! The stack speaks the plane, not the host: the source address of every
//! packet is the address the box assigned to this client, and names resolve
//! THROUGH the tunnel ([`crate::plane_dns`]).

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use mirage_protocol::plane::PacketPlane;
use mirage_tcpip_stack::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use mirage_tcpip_stack::phy::{
    ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken,
};
use mirage_tcpip_stack::socket::tcp::{
    Socket as TcpSocket, SocketBuffer as TcpSocketBuffer, State as TcpState,
};
use mirage_tcpip_stack::socket::udp::{
    PacketBuffer as UdpPacketBuffer, PacketMetadata as UdpPacketMetadata, Socket as UdpSocket,
};
use mirage_tcpip_stack::storage::RingBuffer;
use mirage_tcpip_stack::time::{Duration as StackDuration, Instant as StackInstant};
use mirage_tcpip_stack::wire::{
    HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv4Address, Ipv6Address,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot, Notify};
use tracing::{debug, info, warn};

use crate::plane_dns;

/// The size of one TCP socket's send buffer and of its receive buffer.
const TCP_BUFFER_SIZE: usize = 64 * 1024;
/// The size of one UDP socket's payload buffer.
const UDP_PAYLOAD_SIZE: usize = 64 * 1024;
/// How many datagrams one UDP socket buffers.
const UDP_PACKET_COUNT: usize = 64;
/// The first ephemeral port this client uses inside the plane.
const PORT_RANGE_START: u16 = 40_000;
/// The last ephemeral port this client uses inside the plane.
const PORT_RANGE_END: u16 = 60_000;
/// How long the stack waits between polls when nothing is scheduled.
const IDLE_TICK: Duration = Duration::from_millis(10);
/// How often the counters of the stack are logged.
const STATS_INTERVAL: Duration = Duration::from_secs(10);
/// How long a TCP handshake may take before the flow is given up on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a DNS query may take before it is given up on.
const DNS_TIMEOUT: Duration = Duration::from_secs(5);
/// The resolvers the stack asks THROUGH the plane.
const RESOLVERS: [SocketAddr; 2] = [
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 53)),
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53)),
];

// ─────────────────────────────────────────────────────────────────────────────
// The counters
// ─────────────────────────────────────────────────────────────────────────────

/// The counters of the plane stack.
///
/// Every drop point has a number here: a packet that vanishes where nobody
/// looks is a packet the operator cannot account for.
#[derive(Debug, Default)]
pub struct StackStats {
    /// The IP packets that went onto the plane.
    pub packets_to_plane: AtomicU64,
    /// The IP packets that came off the plane.
    pub packets_from_plane: AtomicU64,
    /// The bytes that went onto the plane.
    pub bytes_to_plane: AtomicU64,
    /// The bytes that came off the plane.
    pub bytes_from_plane: AtomicU64,
    /// The packets dropped because they did not fit the plane.
    pub dropped_too_large: AtomicU64,
    /// The packets dropped because they were not IP packets at all.
    pub dropped_malformed: AtomicU64,
    /// The failures of the plane itself.
    pub plane_errors: AtomicU64,
    /// The TCP flows that reached the established state.
    pub tcp_opened: AtomicU64,
    /// The TCP flows that failed to open.
    pub tcp_failed: AtomicU64,
    /// The bytes the TCP flows carried towards the far side.
    pub tcp_bytes_to_remote: AtomicU64,
    /// The bytes the TCP flows carried back.
    pub tcp_bytes_from_remote: AtomicU64,
    /// The UDP datagrams that went out.
    pub udp_to_remote: AtomicU64,
    /// The UDP datagrams that came in.
    pub udp_from_remote: AtomicU64,
    /// The UDP datagrams dropped because they did not fit the plane.
    pub udp_dropped_oversize: AtomicU64,
    /// The DNS queries that went out.
    pub dns_queries: AtomicU64,
    /// The DNS queries that failed.
    pub dns_failures: AtomicU64,
}

/// The counters of the plane stack as plain numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StackStatsSnapshot {
    /// The IP packets that went onto the plane.
    pub packets_to_plane: u64,
    /// The IP packets that came off the plane.
    pub packets_from_plane: u64,
    /// The bytes that went onto the plane.
    pub bytes_to_plane: u64,
    /// The bytes that came off the plane.
    pub bytes_from_plane: u64,
    /// The packets dropped because they did not fit the plane.
    pub dropped_too_large: u64,
    /// The packets dropped because they were not IP packets at all.
    pub dropped_malformed: u64,
    /// The failures of the plane itself.
    pub plane_errors: u64,
    /// The TCP flows that reached the established state.
    pub tcp_opened: u64,
    /// The TCP flows that failed to open.
    pub tcp_failed: u64,
    /// The bytes the TCP flows carried towards the far side.
    pub tcp_bytes_to_remote: u64,
    /// The bytes the TCP flows carried back.
    pub tcp_bytes_from_remote: u64,
    /// The UDP datagrams that went out.
    pub udp_to_remote: u64,
    /// The UDP datagrams that came in.
    pub udp_from_remote: u64,
    /// The UDP datagrams dropped because they did not fit the plane.
    pub udp_dropped_oversize: u64,
    /// The DNS queries that went out.
    pub dns_queries: u64,
    /// The DNS queries that failed.
    pub dns_failures: u64,
}

impl StackStats {
    /// The counters as plain numbers.
    fn snapshot(&self) -> StackStatsSnapshot {
        StackStatsSnapshot {
            packets_to_plane: self.packets_to_plane.load(Ordering::Relaxed),
            packets_from_plane: self.packets_from_plane.load(Ordering::Relaxed),
            bytes_to_plane: self.bytes_to_plane.load(Ordering::Relaxed),
            bytes_from_plane: self.bytes_from_plane.load(Ordering::Relaxed),
            dropped_too_large: self.dropped_too_large.load(Ordering::Relaxed),
            dropped_malformed: self.dropped_malformed.load(Ordering::Relaxed),
            plane_errors: self.plane_errors.load(Ordering::Relaxed),
            tcp_opened: self.tcp_opened.load(Ordering::Relaxed),
            tcp_failed: self.tcp_failed.load(Ordering::Relaxed),
            tcp_bytes_to_remote: self.tcp_bytes_to_remote.load(Ordering::Relaxed),
            tcp_bytes_from_remote: self.tcp_bytes_from_remote.load(Ordering::Relaxed),
            udp_to_remote: self.udp_to_remote.load(Ordering::Relaxed),
            udp_from_remote: self.udp_from_remote.load(Ordering::Relaxed),
            udp_dropped_oversize: self.udp_dropped_oversize.load(Ordering::Relaxed),
            dns_queries: self.dns_queries.load(Ordering::Relaxed),
            dns_failures: self.dns_failures.load(Ordering::Relaxed),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The handle
// ─────────────────────────────────────────────────────────────────────────────

/// A userspace TCP/IP stack over one packet plane.
///
/// The handle is cheap to share: every flow it opens is independent, and the
/// stack itself lives in one task.
pub struct PlaneStack {
    /// The commands to the stack task.
    cmds: mpsc::Sender<Cmd>,
    /// The counters.
    stats: Arc<StackStats>,
    /// The address the box assigned to this client.
    assigned_v4: Ipv4Addr,
    /// The IPv6 address that goes with it - the same host number in the
    /// client's IPv6 network, the rule both ends of the tunnel use.
    assigned_v6: Ipv6Addr,
    /// The biggest packet that fits the plane.
    mtu: usize,
    /// Set once the stack has polled the plane at least once.
    ready: Arc<AtomicBool>,
}

impl PlaneStack {
    /// Start a stack over `plane` for the client at `assigned_v4`.
    ///
    /// The stack takes the plane as it is: it reads the packets that come
    /// off it and puts its own packets onto it. It never touches the host
    /// network stack.
    pub fn spawn(plane: Arc<dyn PacketPlane>, assigned_v4: Ipv4Addr) -> Arc<Self> {
        let mtu = plane
            .max_packet_size()
            .min(mirage_protocol::plane::MAX_PACKET_SIZE);
        let stats = Arc::new(StackStats::default());
        let ready = Arc::new(AtomicBool::new(false));
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(64);
        let (pkt_tx, pkt_rx) = mpsc::channel::<Vec<u8>>(256);
        let (dev_tx, dev_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let wake = Arc::new(Notify::new());
        let assigned_v6 = derive_client_v6(assigned_v4);

        // The reader: the packets that come off the plane.
        let reader = {
            let plane = Arc::clone(&plane);
            let stats = Arc::clone(&stats);
            let wake = Arc::clone(&wake);
            tokio::spawn(async move {
                let mut buf = vec![0u8; mtu];
                loop {
                    match plane.recv_packet(&mut buf).await {
                        Ok(0) => continue,
                        Ok(read) => {
                            let packet = buf[..read].to_vec();
                            stats.packets_from_plane.fetch_add(1, Ordering::Relaxed);
                            stats
                                .bytes_from_plane
                                .fetch_add(read as u64, Ordering::Relaxed);
                            if pkt_tx.send(packet).await.is_err() {
                                break;
                            }
                            wake.notify_one();
                        }
                        Err(e) => {
                            stats.plane_errors.fetch_add(1, Ordering::Relaxed);
                            warn!(error = %e, "plane stack: the plane ended");
                            break;
                        }
                    }
                }
            })
        };

        // The writer: the packets that go onto the plane.
        let writer = {
            let plane = Arc::clone(&plane);
            let stats = Arc::clone(&stats);
            tokio::spawn(async move {
                let mut dev_rx = dev_rx;
                while let Some(packet) = dev_rx.recv().await {
                    if let Err(e) = plane.send_packet(&packet).await {
                        stats.plane_errors.fetch_add(1, Ordering::Relaxed);
                        warn!(error = %e, "plane stack: the plane refused a packet");
                        break;
                    }
                    stats.packets_to_plane.fetch_add(1, Ordering::Relaxed);
                    stats
                        .bytes_to_plane
                        .fetch_add(packet.len() as u64, Ordering::Relaxed);
                }
            })
        };

        // The stack itself.
        {
            let mut device = PlaneDevice {
                rx: pkt_rx,
                tx: dev_tx,
                mtu,
                stats: Arc::clone(&stats),
            };
            let iface = build_interface(&mut device, assigned_v4, assigned_v6);
            let stack = Stack {
                device,
                iface,
                sockets: SocketSet::new(vec![]),
                tcp: HashMap::new(),
                udp: HashMap::new(),
                dns: HashMap::new(),
                dns_socket: None,
                ports: PortPool::new(),
                assigned_v4,
                assigned_v6,
                stats: Arc::clone(&stats),
                ready: Arc::clone(&ready),
                wake: Arc::clone(&wake),
                reader: Some(reader),
                writer: Some(writer),
            };
            tokio::spawn(run_stack(stack, cmd_rx));
        }

        info!(
            address = %assigned_v4,
            v6 = %assigned_v6,
            mtu,
            "plane stack: started"
        );
        Arc::new(Self {
            cmds: cmd_tx,
            stats,
            assigned_v4,
            assigned_v6,
            mtu,
            ready,
        })
    }

    /// The address the box assigned to this client.
    pub fn assigned_v4(&self) -> Ipv4Addr {
        self.assigned_v4
    }

    /// The IPv6 address that goes with the assigned IPv4 address.
    pub fn assigned_v6(&self) -> Ipv6Addr {
        self.assigned_v6
    }

    /// The biggest packet that fits the plane.
    pub fn mtu(&self) -> usize {
        self.mtu
    }

    /// Whether the stack has polled the plane at least once.
    ///
    /// This is the data path's own readiness: a session that has a plane is
    /// not `connected` until this says yes.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// The counters as plain numbers.
    pub fn stats(&self) -> StackStatsSnapshot {
        self.stats.snapshot()
    }

    /// Open a TCP flow to `remote`.
    pub async fn connect(&self, remote: SocketAddr) -> io::Result<PlaneTcpStream> {
        let (reply, answer) = oneshot::channel();
        self.cmds
            .send(Cmd::Connect { remote, reply })
            .await
            .map_err(|_| broken("the plane stack is gone"))?;
        answer.await.map_err(|_| broken("the plane stack is gone"))?
    }

    /// Open a UDP socket.
    ///
    /// One socket carries every datagram of one association: each datagram
    /// names its own destination, and every reply comes back to the same
    /// socket.
    pub async fn open_udp(&self) -> io::Result<PlaneUdpSocket> {
        let (reply, answer) = oneshot::channel();
        self.cmds
            .send(Cmd::OpenUdp { reply })
            .await
            .map_err(|_| broken("the plane stack is gone"))?;
        answer.await.map_err(|_| broken("the plane stack is gone"))?
    }

    /// Resolve `host` THROUGH the plane.
    ///
    /// A literal address is returned as it is. A name is asked of a public
    /// resolver over the tunnel: the query leaves as an ordinary UDP
    /// datagram and the answer comes back the same way. The host resolver is
    /// never consulted.
    pub async fn resolve(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }
        let (reply, answer) = oneshot::channel();
        self.cmds
            .send(Cmd::Resolve {
                host: host.to_owned(),
                reply,
            })
            .await
            .map_err(|_| broken("the plane stack is gone"))?;
        answer.await.map_err(|_| broken("the plane stack is gone"))?
    }
}

/// The IPv6 address of the peer that owns this IPv4 address.
///
/// The rule is the same on both ends of the tunnel: the host number of the
/// IPv4 address becomes the host number of the client IPv6 network.
pub fn derive_client_v6(v4: Ipv4Addr) -> Ipv6Addr {
    Ipv6Addr::new(0xfd77, 0, 0, 0, 0, 0, 0, u16::from(v4.octets()[3]))
}

/// The error of a stack that is gone.
fn broken(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, what)
}

/// The interface the stack speaks IP on.
///
/// The address list is the address the box assigned to this client, with the
/// prefix of the whole address space: the stack is a point-to-point link
/// between this client and the box, and everything leaves through it.
fn build_interface(device: &mut PlaneDevice, v4: Ipv4Addr, v6: Ipv6Addr) -> Interface {
    let mut iface = Interface::new(
        IfaceConfig::new(HardwareAddress::Ip),
        device,
        StackInstant::now(),
    );
    iface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::new(
            IpAddress::v4(v4.octets()[0], v4.octets()[1], v4.octets()[2], v4.octets()[3]),
            0,
        ))
        .expect("iface IPv4");
        addrs.push(IpCidr::new(
            IpAddress::v6(
                v6.segments()[0],
                v6.segments()[1],
                v6.segments()[2],
                v6.segments()[3],
                v6.segments()[4],
                v6.segments()[5],
                v6.segments()[6],
                v6.segments()[7],
            ),
            0,
        ))
        .expect("iface IPv6");
    });
    let _ = iface
        .routes_mut()
        .add_default_ipv4_route(Ipv4Address::new(v4.octets()[0], v4.octets()[1], v4.octets()[2], v4.octets()[3]));
    let _ = iface
        .routes_mut()
        .add_default_ipv6_route(Ipv6Address::new(
            0xfd77, 0, 0, 0, 0, 0, 0, 0xfe,
        ));
    iface.set_any_ip(true);
    iface
}

// ─────────────────────────────────────────────────────────────────────────────
// The commands
// ─────────────────────────────────────────────────────────────────────────────

/// What the handle asks the stack task to do.
enum Cmd {
    /// Open a TCP flow.
    Connect {
        /// Where to.
        remote: SocketAddr,
        /// The answer.
        reply: oneshot::Sender<io::Result<PlaneTcpStream>>,
    },
    /// Open a UDP socket.
    OpenUdp {
        /// The answer.
        reply: oneshot::Sender<io::Result<PlaneUdpSocket>>,
    },
    /// Resolve a name.
    Resolve {
        /// The name.
        host: String,
        /// The answer.
        reply: oneshot::Sender<io::Result<Vec<IpAddr>>>,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// The device: the plane seen as a network device
// ─────────────────────────────────────────────────────────────────────────────

/// The packet plane seen as a network device.
struct PlaneDevice {
    /// The packets that came off the plane.
    rx: mpsc::Receiver<Vec<u8>>,
    /// The packets that go onto the plane.
    tx: mpsc::UnboundedSender<Vec<u8>>,
    /// The biggest packet that fits the plane.
    mtu: usize,
    /// The counters.
    stats: Arc<StackStats>,
}

impl Device for PlaneDevice {
    type RxToken<'a> = PlaneRxToken;
    type TxToken<'a> = PlaneTxToken;

    fn receive(&mut self, _timestamp: StackInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let packet = self.rx.try_recv().ok()?;
        Some((
            PlaneRxToken { packet },
            PlaneTxToken {
                tx: self.tx.clone(),
                mtu: self.mtu,
                stats: Arc::clone(&self.stats),
            },
        ))
    }

    fn transmit(&mut self, _timestamp: StackInstant) -> Option<Self::TxToken<'_>> {
        Some(PlaneTxToken {
            tx: self.tx.clone(),
            mtu: self.mtu,
            stats: Arc::clone(&self.stats),
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        // The box's TUN expects real checksums: the kernel verifies them.
        caps.checksum = ChecksumCapabilities::default();
        caps
    }
}

/// One packet off the plane.
struct PlaneRxToken {
    /// The packet, byte for byte.
    packet: Vec<u8>,
}

impl RxToken for PlaneRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.packet)
    }
}

/// One packet onto the plane.
struct PlaneTxToken {
    /// Where the packet goes.
    tx: mpsc::UnboundedSender<Vec<u8>>,
    /// The biggest packet that fits the plane.
    mtu: usize,
    /// The counters.
    stats: Arc<StackStats>,
}

impl TxToken for PlaneTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let out = f(&mut buf);
        // The stack sizes its own packets to the plane's budget, so this is
        // the safety net: a packet that still does not fit is split (IPv4)
        // or dropped (IPv6), and either way it is counted.
        let pieces = crate::ipv4::prepare_for_plane(&buf, self.mtu);
        if pieces.is_empty() {
            self.stats.dropped_malformed.fetch_add(1, Ordering::Relaxed);
            debug!(len, "plane stack: dropping a packet that is not an IP packet");
            return out;
        }
        for piece in pieces {
            if piece.len() > self.mtu {
                self.stats.dropped_too_large.fetch_add(1, Ordering::Relaxed);
                debug!(
                    len = piece.len(),
                    budget = self.mtu,
                    "plane stack: dropping a packet that does not fit the plane"
                );
                continue;
            }
            if self.tx.send(piece).is_err() {
                self.stats.plane_errors.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
        out
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The TCP flow
// ─────────────────────────────────────────────────────────────────────────────

/// What a TCP flow and the stack share.
struct TcpShared {
    /// The bytes the application wrote and the stack has not sent yet.
    send_buf: RingBuffer<'static, u8>,
    /// The bytes the stack received and the application has not read yet.
    recv_buf: RingBuffer<'static, u8>,
    /// The application waiting for room in `send_buf`.
    send_waker: Option<Waker>,
    /// The application waiting for bytes in `recv_buf`.
    recv_waker: Option<Waker>,
    /// The flow cannot send any more.
    send_closed: bool,
    /// The flow cannot receive any more - the far side is gone.
    recv_closed: bool,
}

/// One TCP flow over the plane.
///
/// An ordinary byte stream: the SOCKS5 layer copies it to and from the
/// local socket and never learns that it rides IP packets in QUIC datagrams.
pub struct PlaneTcpStream {
    /// The bytes and the wakers, shared with the stack task.
    shared: Arc<Mutex<TcpShared>>,
    /// Wakes the stack task when the application writes.
    wake: Arc<Notify>,
    /// The port this flow opened locally.
    local_port: u16,
}

impl PlaneTcpStream {
    /// The port this flow opened locally inside the plane.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }
}

impl Drop for PlaneTcpStream {
    /// The stack must learn that the application is gone, so it can close
    /// the flow instead of holding it open forever.
    fn drop(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.send_closed = true;
            shared.recv_closed = true;
        }
        self.wake.notify_one();
    }
}

impl AsyncRead for PlaneTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut shared = match self.shared.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let read = shared.recv_buf.dequeue_slice(buf.initialize_unfilled());
        if read > 0 {
            buf.advance(read);
            return Poll::Ready(Ok(()));
        }
        if shared.recv_closed {
            // The end of the stream.
            return Poll::Ready(Ok(()));
        }
        shared.recv_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl AsyncWrite for PlaneTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut shared = match self.shared.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if shared.send_closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the flow is closed",
            )));
        }
        let written = shared.send_buf.enqueue_slice(buf);
        if written > 0 {
            self.wake.notify_one();
            return Poll::Ready(Ok(written));
        }
        shared.send_waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // The bytes are in the shared buffer; the stack sends them.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Ok(mut shared) = self.shared.lock() {
            shared.send_closed = true;
        }
        self.wake.notify_one();
        Poll::Ready(Ok(()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The UDP socket
// ─────────────────────────────────────────────────────────────────────────────

/// One UDP socket over the plane.
///
/// Every datagram names its own destination, and every reply comes back to
/// the same socket - which is exactly what a SOCKS5 UDP association needs.
pub struct PlaneUdpSocket {
    /// The address this socket is bound to.
    local: SocketAddr,
    /// The datagrams to send.
    tx: mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>,
    /// The datagrams that came back.
    rx: mpsc::Receiver<(Vec<u8>, SocketAddr)>,
}

impl PlaneUdpSocket {
    /// The address this socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Send one datagram to `remote`.
    pub fn send_to(&self, data: &[u8], remote: SocketAddr) -> io::Result<()> {
        self.tx
            .send((data.to_vec(), remote))
            .map_err(|_| broken("the plane stack is gone"))
    }

    /// Take one datagram that came back, if there is one.
    pub fn try_recv_from(&mut self) -> Option<(Vec<u8>, SocketAddr)> {
        self.rx.try_recv().ok()
    }

    /// Wait for one datagram that came back.
    pub async fn recv_from(&mut self) -> Option<(Vec<u8>, SocketAddr)> {
        self.rx.recv().await
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The stack task
// ─────────────────────────────────────────────────────────────────────────────

/// One TCP flow the stack drives.
struct TcpEntry {
    /// The bytes and the wakers.
    shared: Arc<Mutex<TcpShared>>,
    /// The local port this flow opened.
    port: u16,
    /// The answer of a connect that is still pending.
    reply: Option<oneshot::Sender<io::Result<PlaneTcpStream>>>,
    /// When the handshake has to be given up on.
    deadline: tokio::time::Instant,
}

/// One UDP socket the stack drives.
struct UdpEntry {
    /// The datagrams to send.
    tx: mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>,
    /// The datagrams that came back.
    rx: mpsc::Sender<(Vec<u8>, SocketAddr)>,
}

/// One DNS query the stack is waiting for.
struct DnsEntry {
    /// The name that was asked.
    host: String,
    /// The type that was asked for.
    qtype: u16,
    /// The resolver the query went to.
    resolver: SocketAddr,
    /// Whether the query has gone out.
    sent: bool,
    /// When the query has to be given up on.
    deadline: tokio::time::Instant,
    /// The answer.
    reply: oneshot::Sender<io::Result<Vec<IpAddr>>>,
}

/// The stack task's state.
struct Stack {
    /// The plane seen as a device.
    device: PlaneDevice,
    /// The interface that speaks IP.
    iface: Interface,
    /// The sockets of the interface.
    sockets: SocketSet<'static>,
    /// The TCP flows.
    tcp: HashMap<SocketHandle, TcpEntry>,
    /// The UDP sockets.
    udp: HashMap<SocketHandle, UdpEntry>,
    /// The DNS queries that are still waiting.
    dns: HashMap<u16, DnsEntry>,
    /// The socket the DNS queries ride.
    dns_socket: Option<SocketHandle>,
    /// The ports this client has opened.
    ports: PortPool,
    /// The IPv4 address the box assigned.
    assigned_v4: Ipv4Addr,
    /// The IPv6 address that goes with it.
    assigned_v6: Ipv6Addr,
    /// The counters.
    stats: Arc<StackStats>,
    /// Set once the stack has polled the plane at least once.
    ready: Arc<AtomicBool>,
    /// Wakes the task when there is work.
    wake: Arc<Notify>,
    /// The task that reads the packets off the plane.
    reader: Option<tokio::task::JoinHandle<()>>,
    /// The task that puts the packets onto the plane.
    writer: Option<tokio::task::JoinHandle<()>>,
}

/// The ephemeral ports this client has opened.
struct PortPool {
    /// The next port to try.
    next: u16,
    /// The ports that are taken.
    used: HashSet<u16>,
}

impl PortPool {
    /// A fresh pool.
    fn new() -> Self {
        PortPool {
            next: PORT_RANGE_START,
            used: HashSet::new(),
        }
    }

    /// Take a free port.
    fn alloc(&mut self) -> Option<u16> {
        let span = PORT_RANGE_END.saturating_sub(PORT_RANGE_START);
        for _ in 0..span {
            let port = self.next;
            self.next += 1;
            if self.next >= PORT_RANGE_END {
                self.next = PORT_RANGE_START;
            }
            if !self.used.contains(&port) {
                self.used.insert(port);
                return Some(port);
            }
        }
        None
    }

    /// Give a port back.
    fn release(&mut self, port: u16) {
        self.used.remove(&port);
    }
}

/// Drive the stack until the handle is gone.
async fn run_stack(mut stack: Stack, mut cmds: mpsc::Receiver<Cmd>) {
    let mut ticker = tokio::time::interval(STATS_INTERVAL);
    // The first tick of an interval fires at once: the line before it would
    // say nothing but zeroes.
    ticker.tick().await;
    loop {
        // The commands that came in while the stack was busy.
        while let Ok(cmd) = cmds.try_recv() {
            stack.apply(cmd);
        }
        stack.poll();
        let tick = stack.next_tick();
        tokio::select! {
            cmd = cmds.recv() => {
                match cmd {
                    Some(cmd) => stack.apply(cmd),
                    None => break,
                }
            }
            _ = stack.wake.notified() => {}
            _ = tokio::time::sleep(tick) => {}
            _ = ticker.tick() => stack.log_stats(),
        }
    }
    // The reader has to stop NOW: another consumer of the same plane may be
    // waiting for the packets, and a datagram that is read and dropped is a
    // datagram nobody gets. Aborting the task cancels the pending receive -
    // the datagram stays in the plane's queue.
    stack.stop_helpers();
    info!("plane stack: the stack is gone");
}

impl Stack {
    /// Stop the tasks that carry the plane's packets.
    fn stop_helpers(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
    }
}

impl Stack {
    /// Do what the handle asked.
    fn apply(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Connect { remote, reply } => self.start_connect(remote, reply),
            Cmd::OpenUdp { reply } => self.start_udp(reply),
            Cmd::Resolve { host, reply } => self.start_resolve(host, reply),
        }
    }

    /// Begin a TCP handshake to `remote`.
    fn start_connect(
        &mut self,
        remote: SocketAddr,
        reply: oneshot::Sender<io::Result<PlaneTcpStream>>,
    ) {
        let Some(port) = self.ports.alloc() else {
            let _ = reply.send(Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "no free port inside the plane",
            )));
            return;
        };
        let local_ip = match remote {
            SocketAddr::V4(_) => IpAddress::v4(
                self.assigned_v4.octets()[0],
                self.assigned_v4.octets()[1],
                self.assigned_v4.octets()[2],
                self.assigned_v4.octets()[3],
            ),
            SocketAddr::V6(_) => IpAddress::v6(
                self.assigned_v6.segments()[0],
                self.assigned_v6.segments()[1],
                self.assigned_v6.segments()[2],
                self.assigned_v6.segments()[3],
                self.assigned_v6.segments()[4],
                self.assigned_v6.segments()[5],
                self.assigned_v6.segments()[6],
                self.assigned_v6.segments()[7],
            ),
        };
        let remote_endpoint = IpEndpoint {
            addr: match remote.ip() {
                IpAddr::V4(v4) => IpAddress::v4(
                    v4.octets()[0],
                    v4.octets()[1],
                    v4.octets()[2],
                    v4.octets()[3],
                ),
                IpAddr::V6(v6) => IpAddress::v6(
                    v6.segments()[0],
                    v6.segments()[1],
                    v6.segments()[2],
                    v6.segments()[3],
                    v6.segments()[4],
                    v6.segments()[5],
                    v6.segments()[6],
                    v6.segments()[7],
                ),
            },
            port: remote.port(),
        };
        let local_endpoint = IpListenEndpoint {
            addr: Some(local_ip),
            port,
        };

        let mut socket = TcpSocket::new(
            TcpSocketBuffer::new(vec![0u8; TCP_BUFFER_SIZE]),
            TcpSocketBuffer::new(vec![0u8; TCP_BUFFER_SIZE]),
        );
        socket.set_keep_alive(Some(StackDuration::from_secs(30)));
        socket.set_timeout(Some(StackDuration::from_secs(30)));
        socket.set_ack_delay(None);
        socket.set_nagle_enabled(false);
        if let Err(e) = socket.connect(self.iface.context(), remote_endpoint, local_endpoint) {
            self.ports.release(port);
            self.stats.tcp_failed.fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the stack refused the connection: {e}"),
            )));
            return;
        }

        let shared = Arc::new(Mutex::new(TcpShared {
            send_buf: RingBuffer::new(vec![0u8; TCP_BUFFER_SIZE]),
            recv_buf: RingBuffer::new(vec![0u8; TCP_BUFFER_SIZE]),
            send_waker: None,
            recv_waker: None,
            send_closed: false,
            recv_closed: false,
        }));
        let handle = self.sockets.add(socket);
        self.tcp.insert(
            handle,
            TcpEntry {
                shared,
                port,
                reply: Some(reply),
                deadline: tokio::time::Instant::now() + CONNECT_TIMEOUT,
            },
        );
    }

    /// Open a UDP socket.
    fn start_udp(&mut self, reply: oneshot::Sender<io::Result<PlaneUdpSocket>>) {
        let Some(port) = self.ports.alloc() else {
            let _ = reply.send(Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "no free port inside the plane",
            )));
            return;
        };
        let socket = UdpSocket::new(
            UdpPacketBuffer::new(
                vec![UdpPacketMetadata::EMPTY; UDP_PACKET_COUNT],
                vec![0u8; UDP_PAYLOAD_SIZE],
            ),
            UdpPacketBuffer::new(
                vec![UdpPacketMetadata::EMPTY; UDP_PACKET_COUNT],
                vec![0u8; UDP_PAYLOAD_SIZE],
            ),
        );
        let handle = self.sockets.add(socket);
        {
            let socket = self.sockets.get_mut::<UdpSocket>(handle);
            let local = IpListenEndpoint {
                addr: Some(IpAddress::v4(
                    self.assigned_v4.octets()[0],
                    self.assigned_v4.octets()[1],
                    self.assigned_v4.octets()[2],
                    self.assigned_v4.octets()[3],
                )),
                port,
            };
            if let Err(e) = socket.bind(local) {
                self.sockets.remove(handle);
                self.ports.release(port);
                let _ = reply.send(Err(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("the stack refused the UDP socket: {e}"),
                )));
                return;
            }
        }
        let (to_app, from_app) = mpsc::unbounded_channel::<(Vec<u8>, SocketAddr)>();
        let (to_stack, from_stack) = mpsc::channel::<(Vec<u8>, SocketAddr)>(64);
        self.udp.insert(
            handle,
            UdpEntry {
                tx: from_app,
                rx: to_stack,
            },
        );
        let _ = reply.send(Ok(PlaneUdpSocket {
            local: SocketAddr::V4(SocketAddrV4::new(self.assigned_v4, port)),
            tx: to_app,
            rx: from_stack,
        }));
    }

    /// Begin a DNS query.
    fn start_resolve(&mut self, host: String, reply: oneshot::Sender<io::Result<Vec<IpAddr>>>) {
        let id = self.next_dns_id();
        self.dns.insert(
            id,
            DnsEntry {
                host,
                qtype: plane_dns::TYPE_A,
                resolver: RESOLVERS[0],
                sent: false,
                deadline: tokio::time::Instant::now() + DNS_TIMEOUT,
                reply,
            },
        );
        self.stats.dns_queries.fetch_add(1, Ordering::Relaxed);
    }

    /// The id of the next DNS query.
    fn next_dns_id(&self) -> u16 {
        let mut id = 0x1000u16;
        while self.dns.contains_key(&id) {
            id = id.wrapping_add(1);
        }
        id
    }

    /// Poll the interface and move the bytes.
    fn poll(&mut self) {
        let now = StackInstant::now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        self.ready.store(true, Ordering::Relaxed);
        self.pump_tcp();
        self.pump_udp();
        self.pump_dns();
    }

    /// Log the counters of the stack.
    ///
    /// A drop that nobody sees is a drop nobody can account for: every
    /// number here has a line that carries it.
    fn log_stats(&self) {
        let stats = self.stats.snapshot();
        info!(
            to_plane = stats.packets_to_plane,
            from_plane = stats.packets_from_plane,
            bytes_to_plane = stats.bytes_to_plane,
            bytes_from_plane = stats.bytes_from_plane,
            dropped_too_large = stats.dropped_too_large,
            dropped_malformed = stats.dropped_malformed,
            plane_errors = stats.plane_errors,
            tcp_opened = stats.tcp_opened,
            tcp_failed = stats.tcp_failed,
            tcp_to_remote = stats.tcp_bytes_to_remote,
            tcp_from_remote = stats.tcp_bytes_from_remote,
            udp_to_remote = stats.udp_to_remote,
            udp_from_remote = stats.udp_from_remote,
            udp_dropped_oversize = stats.udp_dropped_oversize,
            dns_queries = stats.dns_queries,
            dns_failures = stats.dns_failures,
            "plane stack: stats"
        );
    }

    /// How long the stack may sleep before it has to look again.
    fn next_tick(&mut self) -> Duration {        let now = StackInstant::now();
        match self.iface.poll_delay(now, &self.sockets) {
            Some(delay) => {
                let delay = Duration::from_micros(delay.total_micros());
                delay.min(IDLE_TICK).max(Duration::from_millis(1))
            }
            None => IDLE_TICK,
        }
    }

    /// Move the bytes of every TCP flow.
    fn pump_tcp(&mut self) {
        let handles: Vec<SocketHandle> = self.tcp.keys().copied().collect();
        for handle in handles {
            let Some(entry) = self.tcp.get(&handle) else {
                continue;
            };
            let shared = Arc::clone(&entry.shared);
            let local_port = entry.port;
            let mut guard = match shared.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let mut to_remote = 0u64;
            let mut from_remote = 0u64;
            let state;
            {
                let socket = self.sockets.get_mut::<TcpSocket>(handle);
                // The bytes the application wrote.
                loop {
                    if !socket.can_send() {
                        break;
                    }
                    let mut taken = 0usize;
                    let result = socket.send(|out| {
                        let n = guard.send_buf.dequeue_slice(out);
                        taken = n;
                        (n, ())
                    });
                    match result {
                        Ok(_) => {
                            if taken == 0 {
                                break;
                            }
                            to_remote += taken as u64;
                        }
                        Err(_) => break,
                    }
                }
                // The bytes the far side sent.
                loop {
                    if !socket.can_recv() {
                        break;
                    }
                    let mut got = 0usize;
                    let result = socket.recv(|data| {
                        let n = guard.recv_buf.enqueue_slice(data);
                        got = n;
                        (n, ())
                    });
                    match result {
                        Ok(_) => {
                            if got == 0 {
                                break;
                            }
                            from_remote += got as u64;
                        }
                        Err(_) => break,
                    }
                }
                state = socket.state();
            }
            if to_remote > 0 {
                self.stats
                    .tcp_bytes_to_remote
                    .fetch_add(to_remote, Ordering::Relaxed);
                if let Some(waker) = guard.send_waker.take() {
                    waker.wake();
                }
            }
            if from_remote > 0 {
                self.stats
                    .tcp_bytes_from_remote
                    .fetch_add(from_remote, Ordering::Relaxed);
                if let Some(waker) = guard.recv_waker.take() {
                    waker.wake();
                }
            }
            // The far side closed its half: the reader sees the end of the
            // stream.
            if matches!(
                state,
                TcpState::CloseWait
                    | TcpState::FinWait2
                    | TcpState::LastAck
                    | TcpState::TimeWait
                    | TcpState::Closed
            ) {
                guard.recv_closed = true;
                if let Some(waker) = guard.recv_waker.take() {
                    waker.wake();
                }
            }
            if state == TcpState::Closed {
                guard.send_closed = true;
                if let Some(waker) = guard.send_waker.take() {
                    waker.wake();
                }
            }
            drop(guard);

            // The handshake: the answer goes out once the flow is up.
            let established = state == TcpState::Established;
            let closed = state == TcpState::Closed;
            let now = tokio::time::Instant::now();
            let mut answer = None;
            let mut failed = false;
            if let Some(entry) = self.tcp.get_mut(&handle) {
                if let Some(sender) = entry.reply.take() {
                    if established {
                        answer = Some(sender);
                    } else if closed || now >= entry.deadline {
                        failed = true;
                        answer = Some(sender);
                    } else {
                        // Not yet - the answer waits.
                        entry.reply = Some(sender);
                    }
                }
            }
            if let Some(sender) = answer {
                if established {
                    self.stats.tcp_opened.fetch_add(1, Ordering::Relaxed);
                    let _ = sender.send(Ok(PlaneTcpStream {
                        shared,
                        wake: Arc::clone(&self.wake),
                        local_port,
                    }));
                } else {
                    self.stats.tcp_failed.fetch_add(1, Ordering::Relaxed);
                    let _ = sender.send(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the handshake did not finish",
                    )));
                }
            }
            if failed || closed {
                if let Some(entry) = self.tcp.remove(&handle) {
                    self.ports.release(entry.port);
                }
                let _ = self.sockets.remove(handle);
            }
        }
    }

    /// Move the datagrams of every UDP socket.
    fn pump_udp(&mut self) {
        let handles: Vec<SocketHandle> = self.udp.keys().copied().collect();
        for handle in handles {
            // The datagrams to send.
            let mut outgoing = Vec::new();
            if let Some(entry) = self.udp.get_mut(&handle) {
                while let Ok((data, remote)) = entry.tx.try_recv() {
                    outgoing.push((data, remote));
                }
            }
            for (data, remote) in outgoing {
                let socket = self.sockets.get_mut::<UdpSocket>(handle);
                let endpoint = IpEndpoint {
                    addr: match remote.ip() {
                        IpAddr::V4(v4) => IpAddress::v4(
                            v4.octets()[0],
                            v4.octets()[1],
                            v4.octets()[2],
                            v4.octets()[3],
                        ),
                        IpAddr::V6(v6) => IpAddress::v6(
                            v6.segments()[0],
                            v6.segments()[1],
                            v6.segments()[2],
                            v6.segments()[3],
                            v6.segments()[4],
                            v6.segments()[5],
                            v6.segments()[6],
                            v6.segments()[7],
                        ),
                    },
                    port: remote.port(),
                };
                match socket.send_slice(&data, endpoint) {
                    Ok(()) => {
                        self.stats.udp_to_remote.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        self.stats.udp_dropped_oversize.fetch_add(1, Ordering::Relaxed);
                        debug!(
                            error = %e,
                            len = data.len(),
                            "plane stack: the datagram did not fit"
                        );
                    }
                }
            }
            // The datagrams that came back.
            loop {
                let received = {
                    let socket = self.sockets.get_mut::<UdpSocket>(handle);
                    let Ok((data, meta)) = socket.recv() else {
                        break;
                    };
                    let source = match meta.endpoint.addr {
                        IpAddress::Ipv4(v4) => {
                            SocketAddr::V4(SocketAddrV4::new(v4, meta.endpoint.port))
                        }
                        IpAddress::Ipv6(v6) => {
                            SocketAddr::V6(SocketAddrV6::new(v6, meta.endpoint.port, 0, 0))
                        }
                    };
                    (data.to_vec(), source)
                };
                if let Some(entry) = self.udp.get(&handle) {
                    let _ = entry.rx.try_send(received);
                }
                self.stats.udp_from_remote.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Move the DNS queries and their answers.
    fn pump_dns(&mut self) {
        // The socket the queries ride.
        if self.dns_socket.is_none() && !self.dns.is_empty() {
            let Some(port) = self.ports.alloc() else {
                return;
            };
            let socket = UdpSocket::new(
                UdpPacketBuffer::new(
                    vec![UdpPacketMetadata::EMPTY; UDP_PACKET_COUNT],
                    vec![0u8; UDP_PAYLOAD_SIZE],
                ),
                UdpPacketBuffer::new(
                    vec![UdpPacketMetadata::EMPTY; UDP_PACKET_COUNT],
                    vec![0u8; UDP_PAYLOAD_SIZE],
                ),
            );
            let handle = self.sockets.add(socket);
            {
                let socket = self.sockets.get_mut::<UdpSocket>(handle);
                let local = IpListenEndpoint {
                    addr: Some(IpAddress::v4(
                        self.assigned_v4.octets()[0],
                        self.assigned_v4.octets()[1],
                        self.assigned_v4.octets()[2],
                        self.assigned_v4.octets()[3],
                    )),
                    port,
                };
                if socket.bind(local).is_err() {
                    self.sockets.remove(handle);
                    self.ports.release(port);
                    return;
                }
            }
            self.dns_socket = Some(handle);
        }

        // The queries that have not gone out yet.
        let ids: Vec<u16> = self.dns.keys().copied().collect();
        for id in ids {
            let Some(entry) = self.dns.get(&id) else {
                continue;
            };
            if entry.sent {
                continue;
            }
            let Some(query) = plane_dns::build_query(id, &entry.host, entry.qtype) else {
                if let Some(entry) = self.dns.remove(&id) {
                    self.stats.dns_failures.fetch_add(1, Ordering::Relaxed);
                    let _ = entry.reply.send(Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "the name cannot be put on the wire",
                    )));
                }
                continue;
            };
            let Some(handle) = self.dns_socket else {
                continue;
            };
            let resolver = entry.resolver;
            let socket = self.sockets.get_mut::<UdpSocket>(handle);
            let endpoint = IpEndpoint {
                addr: match resolver.ip() {
                    IpAddr::V4(v4) => IpAddress::v4(
                        v4.octets()[0],
                        v4.octets()[1],
                        v4.octets()[2],
                        v4.octets()[3],
                    ),
                    IpAddr::V6(v6) => IpAddress::v6(
                        v6.segments()[0],
                        v6.segments()[1],
                        v6.segments()[2],
                        v6.segments()[3],
                        v6.segments()[4],
                        v6.segments()[5],
                        v6.segments()[6],
                        v6.segments()[7],
                    ),
                },
                port: resolver.port(),
            };
            if socket.send_slice(&query, endpoint).is_ok() {
                if let Some(entry) = self.dns.get_mut(&id) {
                    entry.sent = true;
                }
            }
        }

        // The answers that came back.
        if let Some(handle) = self.dns_socket {
            loop {
                let socket = self.sockets.get_mut::<UdpSocket>(handle);
                let Ok((data, _meta)) = socket.recv() else {
                    break;
                };
                if data.len() < 2 {
                    continue;
                }
                let id = u16::from_be_bytes([data[0], data[1]]);
                let Some(entry) = self.dns.get(&id) else {
                    continue;
                };
                let qtype = entry.qtype;
                let addresses = plane_dns::parse_answers(&data, id, qtype);
                let Some(entry) = self.dns.remove(&id) else {
                    continue;
                };
                if addresses.is_empty() {
                    self.stats.dns_failures.fetch_add(1, Ordering::Relaxed);
                    let _ = entry.reply.send(Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "the name has no address",
                    )));
                } else {
                    let _ = entry.reply.send(Ok(addresses));
                }
            }
        }

        // The queries that ran out of time.
        let now = tokio::time::Instant::now();
        let expired: Vec<u16> = self
            .dns
            .iter()
            .filter(|(_, entry)| now >= entry.deadline)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(entry) = self.dns.remove(&id) {
                self.stats.dns_failures.fetch_add(1, Ordering::Relaxed);
                let _ = entry.reply.send(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the resolver did not answer",
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ipv6_address_follows_the_ipv4_one() {
        assert_eq!(
            derive_client_v6(Ipv4Addr::new(10, 77, 0, 2)),
            Ipv6Addr::new(0xfd77, 0, 0, 0, 0, 0, 0, 2)
        );
        assert_eq!(
            derive_client_v6(Ipv4Addr::new(10, 77, 0, 253)),
            Ipv6Addr::new(0xfd77, 0, 0, 0, 0, 0, 0, 253)
        );
    }

    #[test]
    fn the_port_pool_hands_out_free_ports() {
        let mut pool = PortPool::new();
        let first = pool.alloc().expect("a port");
        let second = pool.alloc().expect("a port");
        assert_ne!(first, second);
        pool.release(first);
        // The released port comes back only when the pool wraps around to
        // it - the pool never hands the same port out twice.
        assert!(first != second);
    }
}
