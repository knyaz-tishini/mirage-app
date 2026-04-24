# smoltcp Analysis: Userspace TCP/IP Stack for TUN→SOCKS5

## What Is smoltcp

smoltcp is a **standalone, event-driven TCP/IP stack** written in Rust, designed for bare-metal and real-time systems. It runs entirely in **userspace** — no kernel TCP involvement. It processes raw IP/Ethernet frames and implements the full TCP state machine, checksums, retransmission, congestion control, and reassembly itself.

- GitHub: https://github.com/smoltcp-rs/smoltcp
- Latest stable: **v0.13.0** (March 20, 2025)
- License: 0-clause BSD (permissive, commercial-safe)
- MSRV: Rust 1.91 stable
- No heap allocation required (uses caller-supplied buffers)
- 4.4k stars, actively maintained

### Protocol Coverage

| Layer | Implemented |
|-------|------------|
| Ethernet II | Yes (unicast, broadcast, multicast, ARP) |
| IPv4 | Yes (routing, fragmentation/reassembly, ICMP) |
| IPv6 | Yes (SLAAC, multicast, 6LoWPAN, ICMPv6) |
| TCP | Yes (congestion control CUBIC+Reno, window scaling, MSS, out-of-order reassembly, keep-alive, zero-window probes) |
| UDP | Yes |
| DNS | Yes |
| DHCPv4 | Yes |
| IGMP | Yes (v1/v2) |

### What Is NOT Implemented

- TCP selective acknowledgements (SACK)
- TCP timestamping
- PLPMTU discovery
- 802.1Q VLANs
- Jumbo frames

For TUN→SOCKS5 use case, none of these omissions matter.

---

## Architecture: Three Key Abstractions

### 1. `Device` Trait (`smoltcp::phy::Device`)

The bottom layer. Represents any raw packet source/sink.

```rust
pub trait Device {
    type RxToken<'a>: RxToken where Self: 'a;
    type TxToken<'a>: TxToken where Self: 'a;

    fn receive(&mut self, timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)>;
    fn transmit(&mut self, timestamp: Instant) -> Option<Self::TxToken<'_>>;
    fn capabilities(&self) -> DeviceCapabilities;
}

pub trait RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R;
}

pub trait TxToken {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R;
}
```

**Key design**: token-based, zero-copy. `receive()` returns a token pair — the TxToken alongside RxToken exists so ICMP replies can be sent without a separate transmit call. No heap. Caller-provided buffer via closure.

`Medium` enum controls framing:
- `Medium::Ethernet` — frames include Ethernet header (14 bytes), ARP handled by smoltcp
- `Medium::Ip` — raw IP packets, no Ethernet header (this is what TUN gives you)

For Wintun/TUN on Windows: use `Medium::Ip`.

### 2. `Interface` (`smoltcp::iface::Interface`)

The network interface. Sits on top of a `Device`. Handles:
- IP address management
- Routing
- ARP/NDP (for Ethernet medium)
- Dispatching ingress frames to sockets
- Draining egress from sockets back to device

```rust
// Construction
let config = Config::new(HardwareAddress::Ip); // for TUN/IP medium
config.random_seed = rand::random();
let mut iface = Interface::new(config, &mut device, Instant::now());

// Set IP address (the "local" address smoltcp claims)
iface.update_ip_addrs(|addrs| {
    addrs.push(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24)).unwrap();
});

// Main poll — processes all queued packets in both directions
iface.poll(Instant::now(), &mut device, &mut sockets);

// Bounded variants (for real-time / avoid unbounded work)
iface.poll_egress(ts, &mut device, &mut sockets);
iface.poll_ingress_single(ts, &mut device, &mut sockets); // one packet
iface.poll_maintenance(ts, &mut sockets);                 // timers only

// Scheduling: when to poll next
let delay: Option<Duration> = iface.poll_delay(Instant::now(), &sockets);
```

`poll()` drives the entire stack. Call it whenever:
- New packets arrive from TUN
- A socket has data to send
- A timer fires (retransmit, keepalive)

### 3. `tcp::Socket` (`smoltcp::socket::tcp::Socket`)

The TCP socket. Pre-allocated buffers, no heap.

```rust
// Create with caller-supplied buffers
let rx_buf = tcp::SocketBuffer::new(vec![0u8; 65535]);
let tx_buf = tcp::SocketBuffer::new(vec![0u8; 65535]);
let socket = tcp::Socket::new(rx_buf, tx_buf);

// Add to SocketSet, get a handle
let mut sockets = SocketSet::new(vec![]);
let handle = sockets.add(socket);

// Listen (passive open) on a port
let sock = sockets.get_mut::<tcp::Socket>(handle);
sock.listen(80).unwrap();

// After poll() drives state machine, check state
let sock = sockets.get::<tcp::Socket>(handle);
if sock.may_recv() {
    let n = sock.recv_slice(&mut buf).unwrap();
}
if sock.may_send() {
    sock.send_slice(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
}

// State inspection
sock.state()            // State enum: Listen, SynReceived, Established, etc.
sock.remote_endpoint()  // Option<IpEndpoint> — peer address+port
sock.local_endpoint()   // Option<IpEndpoint>
sock.is_open()
sock.is_active()
sock.may_recv()         // data available OR FIN not yet received
sock.may_send()         // send buffer has space AND connection not closing

// Shutdown
sock.close();   // graceful FIN
sock.abort();   // RST
```

---

## TUN→SOCKS5 Pattern: How tun2proxy / netstack-smoltcp Do It

### The Core Insight

smoltcp sees raw IP packets. TUN delivers raw IP packets. The bridge:

1. Read IP packet from Wintun
2. Feed into smoltcp `Device::receive()` via your custom impl
3. Call `iface.poll()` → smoltcp runs TCP state machine
4. Detect new `Established` connections on smoltcp sockets
5. For each new connection: open a SOCKS5 connection to upstream proxy
6. Bidirectionally relay: smoltcp socket ↔ SOCKS5 TCP stream
7. Packets smoltcp wants to send → write back to Wintun

### Custom Device Implementation for Wintun

```rust
use std::collections::VecDeque;

pub struct WintunDevice {
    // inbound: packets read from Wintun, waiting for smoltcp to consume
    rx_queue: VecDeque<Vec<u8>>,
    // outbound: packets smoltcp wants to send, waiting to write to Wintun
    tx_queue: VecDeque<Vec<u8>>,
    mtu: usize,
}

pub struct RxToken(Vec<u8>);
pub struct TxToken<'a>(&'a mut VecDeque<Vec<u8>>, usize);

impl smoltcp::phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl<'a> smoltcp::phy::TxToken for TxToken<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

impl smoltcp::phy::Device for WintunDevice {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _ts: smoltcp::time::Instant)
        -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)>
    {
        self.rx_queue.pop_front().map(|pkt| {
            (RxToken(pkt), TxToken(&mut self.tx_queue, self.mtu))
        })
    }

    fn transmit(&mut self, _ts: smoltcp::time::Instant)
        -> Option<Self::TxToken<'_>>
    {
        Some(TxToken(&mut self.tx_queue, self.mtu))
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.medium = smoltcp::phy::Medium::Ip; // TUN = raw IP, no Ethernet header
        caps.max_transmission_unit = self.mtu;
        caps
    }
}
```

Then the event loop:

```rust
// Push a packet from Wintun into the device
device.rx_queue.push_back(wintun_packet_bytes);

// Drive smoltcp
iface.poll(Instant::now(), &mut device, &mut sockets);

// Drain packets smoltcp wants to send back out through Wintun
while let Some(pkt) = device.tx_queue.pop_front() {
    wintun_session.send_packet(&pkt);
}

// Check all sockets for new connections
for handle in socket_handles.iter() {
    let sock = sockets.get::<tcp::Socket>(*handle);
    if sock.state() == tcp::State::Established && !already_proxying(handle) {
        let dst = sock.remote_endpoint().unwrap();
        spawn_socks5_relay(*handle, dst);
    }
}
```

### netstack-smoltcp: The Ready-Made Bridge

`netstack-smoltcp` (crate: `netstack-smoltcp`, GitHub: `cavivie/netstack-smoltcp`) wraps exactly this pattern into a Tokio-async API:

```rust
use netstack_smoltcp::StackBuilder;

let (stack, runner, udp_socket, tcp_listener) = StackBuilder::default()
    .stack_buffer_size(512)
    .tcp_buffer_size(4096)
    .enable_tcp(true)
    .enable_udp(true)
    .mtu(1500)
    .build()?;

// stack is Sink<AnyIpPktFrame> — push Wintun packets in
// stack is also Stream<AnyIpPktFrame> — pull packets to write back to Wintun
// tcp_listener is Stream<TcpStream> — yields one TcpStream per new TCP connection
// runner must be spawned: tokio::spawn(runner);

// Accept loop — each item is a smoltcp-backed TcpStream with AsyncRead+AsyncWrite
while let Some(tcp_stream) = tcp_listener.next().await {
    let dst = tcp_stream.local_addr(); // original destination (what the app wanted)
    tokio::spawn(forward_to_socks5(tcp_stream, dst, socks5_addr));
}
```

Internal architecture of `TcpListener` in netstack-smoltcp:
- Detects SYN packets (syn=1, ack=0)
- Creates smoltcp `tcp::Socket`, adds to SocketSet, calls `listen()`
- Wraps socket in a `TcpStream` with dual `RingBuffer` (send + recv)
- Waker-based async: `Waker` stored in socket state, invoked when buffer changes
- Hands `TcpStream` off via unbounded channel to the `next()` stream

---

## Feeding Wintun Packets into smoltcp

Wintun (Windows TUN driver) delivers packets via `Session::receive_packet()` returning `&[u8]`. The integration:

```rust
// Read loop (separate thread or task)
loop {
    let pkt = session.receive_blocking()?;   // blocks until packet arrives
    let bytes = pkt.bytes().to_vec();
    device_rx_tx.send(bytes).await?;         // send to smoltcp driver task
    // OR: device.rx_queue.push_back(bytes); iface.poll(...)
}
```

Key points for Wintun:
- Packets are raw IPv4/IPv6 — no Ethernet header
- Use `Medium::Ip` in `DeviceCapabilities`
- MTU: match Wintun session MTU (typically 1500 or configured value)
- Wintun sends complete IP packets, smoltcp handles fragmentation internally

---

## Extracting TCP Connections for SOCKS5 Forwarding

Two strategies:

### Strategy A: Manual socket management (raw smoltcp)

Pre-create N listening sockets (smoltcp has no dynamic socket creation during poll):

```rust
// Before the loop: create a pool of listening sockets on all ports
// smoltcp will match incoming SYN to first available listening socket
for _ in 0..MAX_CONNECTIONS {
    let sock = tcp::Socket::new(rx_buf(), tx_buf());
    let h = sockets.add(sock);
    sockets.get_mut::<tcp::Socket>(h).listen(0)?; // port 0 = any
    pool.push(h);
}

// In the poll loop:
for h in &pool {
    let sock = sockets.get::<tcp::Socket>(*h);
    if sock.state() == tcp::State::Established {
        let remote = sock.remote_endpoint().unwrap();
        let local = sock.local_endpoint().unwrap();
        // local.port = original destination port (before NAT rewrite)
        connect_socks5_and_relay(*h, remote, local);
        // replace socket with a fresh listening one
    }
}
```

### Strategy B: Use netstack-smoltcp (recommended)

`TcpListener` stream handles all socket lifecycle. Each `TcpStream` item:
- Implements `tokio::io::AsyncRead + AsyncWrite`
- `local_addr()` = original destination (what the proxied app connected to)
- `peer_addr()` = source IP:port of the connection

```rust
async fn forward_to_socks5(
    mut smoltcp_stream: TcpStream,
    dst: SocketAddr,
    proxy: SocketAddr,
) {
    let mut socks = Socks5Stream::connect(proxy, dst).await.unwrap();
    tokio::io::copy_bidirectional(&mut smoltcp_stream, &mut socks).await.ok();
}
```

---

## Performance Characteristics

| Metric | Value |
|--------|-------|
| Loopback benchmark | ~15.4 Gbps |
| TAP writer throughput | ~7.9 Gbps |
| TAP reader throughput | ~3.7 Gbps |
| vs. Linux TCP (loopback) | ~1 Gbps comparable |

**Practical notes:**
- Buffer sizing is critical: throughput peaks at an "ideal" buffer size, degrades sharply beyond it (open issue #949). Must tune per-connection latency.
- Single-threaded poll model: `iface.poll()` is not `Send`. Either use one thread for smoltcp or wrap in `Mutex`.
- `netstack-smoltcp` + `tun-rs` (with GSO/GRO on Linux) claims 4x improvement over alternatives. Windows Wintun has no GSO so this does not apply.
- No async runtime integration in smoltcp itself — it's synchronous/event-driven. `netstack-smoltcp` adds the Tokio bridge.

---

## Maturity and Production Readiness

| Aspect | Assessment |
|--------|-----------|
| Core TCP/IP | Production-ready for normal traffic |
| Embedded deployments | Actively used (ARTIQ, ionpak hardware) |
| Userspace proxy use | Proven: tun2proxy, netstack-smoltcp |
| Maintenance | Active (v0.13.0 Mar 2025, Edition 2024) |
| RFC compliance | Good, improving (zero-window probes, retransmit fixes in recent releases) |
| Missing: SACK | Will reduce throughput on lossy/high-latency paths |
| Missing: timestamps | Minor for proxy use |
| Windows support | Yes (via `netstack-smoltcp` + Wintun) |
| Async/Tokio | Via `netstack-smoltcp` wrapper only |

**Known production users:**
- tun2proxy — TUN→SOCKS5/HTTP proxy (Rust)
- ARTIQ — scientific hardware controller
- Various embedded networking projects

---

## Relevant Crates

| Crate | Role |
|-------|------|
| `smoltcp` v0.13.0 | Core TCP/IP stack |
| `netstack-smoltcp` | Tokio-async TUN→TcpStream bridge over smoltcp |
| `tun-rs` | TUN device (Linux/macOS/Windows, GSO/GRO on Linux) |
| `wintun` | Wintun kernel driver wrapper for Windows |

---

## Applicability to mirage-client-core

mirage-client-core already has `tun/nat_bridge/` with TCP/UDP handlers and a NAT-based approach (`nat.rs`, `dispatcher.rs`). smoltcp offers an alternative:

**Current approach** (NAT bridge): Intercept packets at IP level, rewrite dst→local proxy port, let OS TCP handle connections, use `SO_ORIGINAL_DST` or NAT table to recover original destination.

**smoltcp approach**: Implement full userspace TCP. No OS TCP involved for proxied connections. Cleaner on Windows where `SO_ORIGINAL_DST` equivalent is absent and NAT table tricks are fragile.

`netstack-smoltcp` is the minimal integration path: replace `tun/stack.rs` device loop with a `StackBuilder`, accept `TcpStream`s from `tcp_listener`, forward each to existing SOCKS5 logic in `socks5.rs`.

---

## Sources

- [smoltcp GitHub](https://github.com/smoltcp-rs/smoltcp)
- [smoltcp docs.rs](https://docs.rs/smoltcp/latest/smoltcp/)
- [smoltcp::phy::Device](https://docs.rs/smoltcp/latest/smoltcp/phy/trait.Device.html)
- [smoltcp::socket::tcp::Socket](https://docs.rs/smoltcp/latest/smoltcp/socket/tcp/struct.Socket.html)
- [smoltcp releases](https://github.com/smoltcp-rs/smoltcp/releases)
- [netstack-smoltcp (cavivie fork)](https://github.com/cavivie/netstack-smoltcp)
- [netstack-smoltcp (automesh)](https://github.com/automesh-network/netstack-smoltcp)
- [tun2proxy](https://github.com/tun2proxy/tun2proxy)
- [smoltcp performance issue #949](https://github.com/smoltcp-rs/smoltcp/issues/949)
