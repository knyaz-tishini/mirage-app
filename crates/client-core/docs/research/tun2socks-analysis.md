# tun2socks (xjasonlyu) — Architecture Analysis

Source: https://github.com/xjasonlyu/tun2socks  
Version: v2.6.0 (June 2025), Go 1.26, MIT license

---

## 1. What It Does

Captures all TCP/UDP traffic from a TUN virtual NIC, runs it through a **full userspace TCP/IP stack** (gVisor netstack), and proxies each connection to a SOCKS5 (or HTTP/Shadowsocks/SSH) server. Applications need zero modification — they see a normal network interface.

---

## 2. TUN → SOCKS5 Flow

```
App syscall (connect/send)
  │
  ▼
OS kernel routes packet → TUN device (tun0 / wintun)
  │  raw IP packets
  ▼
WireGuard tun.NativeTun  ──read loop──►  iobased.Endpoint (adapter)
  │
  ▼
gVisor netstack  (full TCP/IP state machine in userspace)
  │  parses IP → TCP/UDP headers, maintains connection state
  ▼
TCP Forwarder (core/tcp.go)  — completes 3-way handshake on behalf of dest
  │
  ▼
tunnel/tcp.go: handleTCPConn
  │  extract metadata (src IP:port, dst IP:port)
  ▼
proxy/socks5/ — dial SOCKS5 server with destination
  │
  ▼
io.CopyBuffer bidirectional relay  (buffer pool, TCP half-close)
  │
  ▼
SOCKS5 server → real destination
```

UDP path is analogous: `core/udp.go` UDP forwarder → `tunnel/udp.go` → proxy dial.

---

## 3. gVisor Netstack — What and Why

### What

gVisor netstack (`gvisor.dev/gvisor`) is Google's complete TCP/IP stack written in Go, originally built for the gVisor sandbox kernel. It implements IPv4/IPv6, TCP, UDP, ICMP entirely in userspace — no kernel involvement beyond reading/writing raw packets to the TUN fd.

Protocols registered at stack init (`core/stack.go`):
```go
NetworkProtocols: []stack.NetworkProtocolFactory{
    ipv4.NewProtocol,
    ipv6.NewProtocol,
}
TransportProtocols: []stack.TransportProtocolFactory{
    tcp.NewProtocol,
    udp.NewProtocol,
    icmp.NewProtocol4,
    icmp.NewProtocol6,
}
```

### Why Userspace Stack vs Raw Packet Rewriting

**Naive SYN-rewriting** approach:
- Intercept SYN packet, rewrite dst IP:port → local listener
- Local listener completes handshake, opens proxy connection
- Problem: TCP state lives in the OS kernel. You must keep kernel state and proxy state in sync. ACK numbers, window sizes, retransmit timers, RST handling — all must match both sides.
- Any mismatch → RST storm, stale half-open connections, data corruption.
- Does not handle TIME_WAIT, FIN/FIN-ACK sequences, out-of-order segments, keepalives.

**gVisor netstack** approach:
- The stack **owns the TCP state machine entirely**. No kernel TCP state exists for these connections — gVisor is the TCP peer.
- Handles retransmits, window scaling, keepalives, RST, FIN sequences correctly by design (full RFC implementation).
- Promiscuous mode + address impersonation: stack accepts packets for **any** IP, responds as if it owns every address.
- Forwarder completes the 3-way handshake synchronously → only then opens SOCKS5 connection → no state desync possible.
- Edge cases handled: connection refused → proper RST back to app; SOCKS5 error → clean FIN to app.

**Concrete reliability wins**:
| Scenario | SYN-rewrite | gVisor |
|---|---|---|
| Retransmit during proxy connect | Kernel retransmits to wrong endpoint | Netstack buffers, handles internally |
| App sends data before handshake completes | Kernel queues in socket buffer; desync risk | gVisor buffers in its own stack |
| RST from SOCKS server | Must inject RST into kernel — race condition | netstack sends RST to app directly |
| Window scaling negotiation | Must track and replay — fragile | Handled by netstack transparently |
| IPv6 → IPv4 translation | Manual packet rewrite | netstack does dual-stack natively |

---

## 4. DNS Handling

File: `dns/resolver.go`

```go
func init() {
    net.DefaultResolver.PreferGo = true
    net.DefaultResolver.Dial = dialer.DialContext
}
```

Two lines do all the work:

1. `PreferGo = true` — forces Go's pure-Go DNS resolver (not CGO/system stub resolver). Ensures DNS queries go through Go's `net.Resolver` pipeline, not libc.
2. `Dial = dialer.DialContext` — takes over the DNS transport. All DNS UDP/TCP connections now go through tun2socks's dialer, which routes them through the TUN interface and out the SOCKS5 proxy (or direct, depending on config).

**Result**: DNS queries are transparently tunneled alongside TCP/UDP traffic. No separate DNS interception rule needed. DNS-over-SOCKS5 is automatic.

Note: this handles DNS for the tun2socks process itself (e.g., resolving proxy server hostname). Traffic from other processes goes through the TUN device as raw packets — their DNS is captured by gVisor + UDP forwarder → proxy just like any UDP.

---

## 5. Windows Support

**Driver**: Windows has no native TUN. tun2socks uses **Wintun** — the same kernel driver used by WireGuard on Windows.

File: `core/device/tun/tun_wireguard_windows.go`
```go
func createTUN(name string, mtu int) (tun.Device, error) {
    return tun.CreateTUN(name, mtu)  // delegates to golang.zx2c4.com/wireguard
}
```

Dependencies in `go.mod`:
- `golang.zx2c4.com/wireguard` — Go WireGuard library (TUN abstraction)
- `golang.zx2c4.com/wintun` — Wintun Windows kernel driver

**Adapter chain on Windows**:
```
Wintun kernel driver (ring buffer IPC)
  → wireguard-go NativeTun (Go wrapper)
  → tun2socks TUN struct (adds iobased.Endpoint adapter)
  → gVisor iobased.Endpoint
  → gVisor netstack
```

Wintun uses a shared memory ring buffer between kernel and userspace — much faster than ioctl-based approaches. No TAP driver, no OpenVPN adapter needed.

**Platform matrix**:
| Platform | TUN driver |
|---|---|
| Linux | `/dev/net/tun` via `tun_netstack.go` (fdbased endpoint, `Readv` dispatch) |
| Windows | Wintun via `tun_wireguard_windows.go` |
| macOS/BSD | WireGuard userspace TUN via `tun_wireguard_unix.go` |

---

## 6. Key Source Files

| File | Role |
|---|---|
| `main.go` | CLI entry point, flag parsing, calls `engine.Insert()` + `engine.Start()` |
| `engine/engine.go` | Orchestrator: sets up logger, REST API, then calls `core/` to build netstack + TUN |
| `core/stack.go` | Creates gVisor stack with IPv4/IPv6/TCP/UDP/ICMP, registers transport handlers before NIC creation (race prevention) |
| `core/nic.go` | Creates NIC from LinkEndpoint; enables promiscuous mode + address impersonation; handles multicast |
| `core/tcp.go` | `withTCPHandler` — gVisor TCP forwarder; completes 3-way handshake; wraps as `tcpConn`; sets keepalive (60s idle, 30s interval, 9 probes) |
| `core/udp.go` | `withUDPHandler` — gVisor UDP forwarder; wraps as `udpConn` with `TransportEndpointID` |
| `core/route.go` | IP route configuration for the stack |
| `core/device/tun/tun.go` | TUN device interface definition |
| `core/device/tun/tun_wireguard.go` | WireGuard TUN → `iobased.Endpoint` adapter (cross-platform) |
| `core/device/tun/tun_wireguard_windows.go` | Windows Wintun creation |
| `core/device/tun/tun_netstack.go` | Linux native TUN via `fdbased.New()` (faster on Linux) |
| `tunnel/tcp.go` | `handleTCPConn`: extracts metadata, dials proxy, runs `pipe()` bidirectional relay |
| `tunnel/udp.go` | UDP association handling |
| `tunnel/tunnel.go` | Global handler registration |
| `proxy/socks5/` | SOCKS5 client implementation |
| `proxy/proxy.go` | Proxy interface + factory |
| `dns/resolver.go` | Forces Go resolver + routes DNS through custom dialer |
| `dialer/` | Network dialer with interface binding, routing mark, UDP timeout |
| `metadata/` | Connection metadata struct (src/dst addr, proto, network) |

---

## 7. Why More Reliable Than Naive SYN-Rewriting

### The fundamental problem with packet rewriting

Rewriting packets (change dst IP in SYN, redirect to local port) forces **two TCP state machines** to stay synchronized:
1. Kernel's TCP stack (talking to the app)
2. The proxy relay code (talking to the SOCKS server)

These diverge constantly:
- App retransmits SYN — kernel has its own timer; relay may have already opened connection
- Proxy server slow to respond — kernel timer fires RST before relay connects
- Window scale factor negotiated in SYN/SYN-ACK must be intercepted and retransmitted correctly
- FIN/FIN-ACK half-close sequences must be tracked and relayed exactly
- ACK sequence numbers must be translated if any byte-level insertion happened

### The gVisor solution: own the TCP state entirely

gVisor acts as the **TCP endpoint** for the app. The kernel has no TCP state for these connections — from kernel's perspective, data goes to/from a TUN fd. gVisor is the peer.

- App sends SYN → gVisor completes handshake → app thinks connection is established
- gVisor buffers app's data → dials SOCKS5 in parallel
- Once SOCKS5 connected → relay begins, draining gVisor's buffer
- App's retransmit timers see ACKs from gVisor, not the remote server — decoupled
- SOCKS5 failure → gVisor sends clean RST to app; no kernel state to clean up

This is exactly the architecture that WireGuard, Tailscale (userspace mode), and other production tools use when they need reliable TUN-based proxying.

---

## 8. Proxy Models Supported

From `proxy/` directory structure:
- `socks5/` — SOCKS5 with auth
- `socks4/` — SOCKS4
- `http/` — HTTP CONNECT proxy
- `shadowsocks/` — Shadowsocks (obfuscated)
- `ssh/` — SSH tunnel
- `relay/` — go-gost relay protocol
- `direct/` — bypass (no proxy)
- `reject/` — block

---

## Sources

- [xjasonlyu/tun2socks GitHub](https://github.com/xjasonlyu/tun2socks)
- [gVisor Networking Architecture](https://gvisor.dev/docs/architecture_guide/networking/)
- [gVisor Networking Security](https://gvisor.dev/blog/2020/04/02/gvisor-networking-security/)
- [Userspace WireGuard tunnels with Netstack](https://ryan-schachte.com/blog/userspace_wireguard_tunnels/)
- [DeepWiki tun2socks architecture](https://deepwiki.com/xjasonlyu/tun2socks)
- [Tailscale: Kernel vs userspace routers](https://tailscale.com/kb/1177/kernel-vs-userspace-routers)
