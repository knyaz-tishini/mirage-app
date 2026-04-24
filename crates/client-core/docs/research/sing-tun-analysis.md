# sing-tun Architecture Analysis

Source: https://github.com/SagerNet/sing-tun  
License: GPL-3.0 (nekohasekai)  
Language: Go 98%, Assembly 2%  
Used by: sing-box, Clash.Meta, SagerNet Android app, millions of anti-censorship users

---

## 1. What sing-tun Does

sing-tun creates a virtual TUN network interface that captures all outgoing IP packets from the OS,
translates them from L3 (raw IP) to L4 (TCP/UDP connections), and hands them to a proxy handler.
The proxy handler forwards them to a remote proxy server. Responses flow back the same path in reverse.

This gives transparent proxying: apps need zero configuration. All traffic is captured at the IP layer.

---

## 2. Windows-Specific Architecture

### 2.1 Wintun Driver

On Windows, raw TUN access requires a kernel driver. sing-tun uses **Wintun** (by WireGuard team):

- Kernel-mode NDIS driver, distributed as `wintun.dll`
- Exposes a simple userspace API: allocate session, receive/send packet buffers
- Operates at Layer 3 (IP packets only — no Ethernet headers)
- Code-signed binaries for AMD64, x86, ARM64, ARM32
- GPL-2.0 source, permissive license for precompiled signed DLLs

sing-tun wraps Wintun via `golang.zx2c4.com/wintun` (the official Go bindings).

### 2.2 NativeTun struct (tun_windows.go)

```go
type NativeTun struct {
    adapter     *wintun.Adapter
    session     wintun.Session
    readWait    windows.Handle   // event signaled when packets arrive
    fwpm        *wfp.Session     // Windows Filtering Platform session
    // ... rate juggler, options
}
```

**Lifecycle:**
1. `New()` — generates GUID from device name (MD5 hash for determinism), creates/opens Wintun adapter
2. `configure()` — assigns IPv4/IPv6 addresses via LUID, sets MTU, DNS servers, disables router discovery
3. `Start()` — if `AutoRoute=true`: adds routes; if `StrictRoute=true`: installs WFP filters
4. `Close()` — removes WFP filters, flushes DNS cache, closes session and adapter

### 2.3 Packet I/O on Windows

Three read patterns for different consumers:

```go
// Basic: copies into caller's buffer
func (t *NativeTun) Read(buf []byte, offset int) (int, error)

// Zero-copy: returns slice into Wintun's ring buffer + release callback
func (t *NativeTun) ReadPacket() ([]byte, func(), error)

// Callback: caller processes in-place, no copy
func (t *NativeTun) ReadFunc(fn func([]byte) bool) (bool, error)
```

Write always calls `wintun.Session.SendPacket()`.

### 2.4 Rate Juggler (spinloop optimization)

At high data rates, blocking on the Windows read event adds latency from kernel transitions.
The `rateJuggler` measures throughput and switches to busy-wait (spinloop) when bandwidth > ~100 Mbps.
At low rates it blocks on the event handle to avoid burning CPU.

This is a production detail naive implementations miss entirely.

### 2.5 StrictRoute via WFP (Windows Filtering Platform)

`StrictRoute=true` installs WFP filter rules at the kernel level:

1. **Permit filter** — allows packets from the sing-box process itself (by PID/AppID)
2. **DNS block filter** — blocks DNS queries (port 53) going out any interface *except* the TUN
3. **Catch-all block filter** — lower priority, blocks all outbound traffic not matching above

Effect: the OS cannot leak DNS queries via the physical NIC's DNS. All DNS goes through TUN.
This directly prevents Windows' "multihomed DNS resolution" — a Windows-specific DNS leak vector
where Windows sends DNS queries to all available interfaces simultaneously and takes the fastest answer.

WFP operates at `FWPM_LAYER_ALE_AUTH_CONNECT_V4/V6` and `FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4/V6`.

**Tradeoff:** StrictRoute may break VirtualBox host-only adapters and some VM networking.

### 2.6 Route Management

Uses Windows LUID (Locally Unique Interface Identifier) instead of interface name strings.
LUID is stable across renames. Routes added via `golang.org/x/net/route` and Windows IP Helper API.

Auto-route adds:
- `0.0.0.0/1` and `128.0.0.0/1` (splits default route, avoids overriding the original default gateway)
- `::0/1` and `8000::/1` for IPv6

Splitting into two /1 routes instead of one /0 route is intentional: the two /1 routes have longer
prefix length than the original /0 default route, so they win in metric without deleting the original.
This preserves the ability to reach the VPN server itself via the original gateway.

### 2.7 Network Monitor (monitor_windows.go)

Watches for network topology changes:
- Registers route change callback via `NotifyRouteChange2`
- Registers interface change callback via `NotifyIpInterfaceChange`
- Scans `GetIpForwardTable2` to find current default gateway (lowest-metric non-virtual interface)
- Emits `DefaultInterfaceUpdated` events to consumers

sing-box uses this to rebind the outbound socket when the default route changes (e.g. WiFi → Ethernet).

---

## 3. TCP/IP Stack Options

sing-tun implements three stack modes. The stack's job: take raw IP packets from Wintun, assemble
TCP streams or UDP datagrams, and hand them to the proxy handler as `net.Conn`-like objects.

### 3.1 System Stack (stack_system.go + stack_system_windows.go)

**How it works:**
1. Read raw IP packet from Wintun
2. Inspect header: IPv4/IPv6, protocol (TCP/UDP/ICMP)
3. For TCP: maintain a NAT table (source addr:port → allocated local port). Rewrite dest IP/port
   to `127.0.0.1:<local_listener_port>`, fix checksums, write back into TUN.
   The OS kernel receives this "incoming" packet and delivers it to a local TCP listener.
   That listener hands the `net.Conn` to the proxy handler.
4. For UDP: similar NAT with timeout-based cleanup.
5. For ICMP: direct handling or echo relay.

**Windows loop** (stack_system.go uses WinTun read path):
```
loop:
  packet = tun.ReadPacket()
  version = packet[0] >> 4
  switch version {
    case 4: processIPv4(packet)
    case 6: processIPv6(packet)
  }
  release()
```

**Pros:** Low CPU. Kernel does all TCP/IP work.  
**Cons:** Requires a local listener socket per connection type. Some OS networking behaviors leak through.

### 3.2 gVisor Stack (stack_gvisor.go)

Uses `github.com/sagernet/gvisor` — a fork of `google/gvisor` netstack, pruned to only the
`pkg/tcpip` networking components (no container/sandbox parts). Tag `sing-box-mod` indicates
SagerNet patches for performance and API compatibility.

**How it works:**
1. WintunEndpoint (tun_windows_gvisor.go) bridges Wintun ↔ gVisor:
   - `dispatchLoop()` reads packets from Wintun, calls `e.dispatcher.DeliverNetworkPacket()`
   - `WritePackets()` sends gVisor-processed packets back to Wintun
2. gVisor's full TCP/IP stack processes packets in userspace: handles SYN/ACK, retransmit,
   congestion control, etc.
3. TCPForwarder (stack_gvisor_tcp.go) intercepts new TCP connections, creates `net.Conn`-like
   object, passes to proxy handler
4. UDPForwarder does same for UDP datagrams
5. ICMPForwarder handles pings

**TCP buffer sizes:**
- iOS: reduced buffers (iOSBufferDefault / iOSBufferMax) to fit iOS memory limits
- Windows + other: standard gVisor defaults

**Loopback handling:** `NewTCPForwarderWithLoopback()` — when packet dst matches a configured
loopback address, swaps src/dst IPs and recalculates checksum before reinserting. This handles
traffic that originates from the proxy process itself (avoids infinite loop).

**Pros:** Complete isolation from OS stack. Excellent compatibility. No kernel TCP behaviors leaking.  
**Cons:** Higher CPU (full userspace TCP/IP). gVisor is a large dependency (~24k commits in fork).

**Note on "gVisor is Linux-only":** This refers to the full gVisor container runtime. The netstack
(tcpip package) runs fine on Windows. sing-box's confusion around this was a documentation issue, not
a technical limitation.

### 3.3 Mixed Stack (stack_mixed.go)

Default when gVisor build tag is present.

**Architecture:**
```
Wintun → Mixed.processPacket()
  TCP/ICMP → System handler (faster path, kernel does TCP)
  UDP       → gVisor endpoint.InjectInbound() → gVisor UDP forwarder
```

A `packetLoop()` goroutine reads gVisor-processed egress packets and writes them back to Wintun.

**Rationale:** UDP benefits most from gVisor's full stack handling (DNS, QUIC, gaming traffic).
TCP via system stack is faster and sufficient for most use cases.

---

## 4. Packet Flow (Windows, Mixed Stack)

```
Application
    |
    | (normal TCP/IP to e.g. 1.2.3.4:443)
    v
Windows kernel IP routing
    |
    | (route 0.0.0.0/1 and 128.0.0.0/1 → TUN interface)
    v
Wintun kernel driver
    |
    | (raw IP packet in ring buffer)
    v
NativeTun.ReadPacket() [userspace, sing-tun]
    |
    v
Mixed.processPacket()
    |
    +--[TCP]--→ System NAT table lookup/create
    |               Rewrite dst → 127.0.0.1:local_port
    |               Recalculate checksum
    |               Write back to Wintun
    |               Kernel delivers to local TCP listener
    |               net.Conn → PrepareConnection() → proxy handler
    |
    +--[UDP]--→ gVisor endpoint.InjectInbound()
                    gVisor UDP forwarder
                    PacketConn → PrepareConnection() → proxy handler
                    Response packets → WritePackets() → Wintun
```

DNS flow (with StrictRoute + DNS hijacking configured in sing-box):
```
App DNS query (UDP port 53)
    → TUN (caught by WFP, cannot escape via physical NIC)
    → gVisor UDP path
    → sing-box DNS server (fake-IP or real resolver)
    → response written back through TUN to app
```

---

## 5. DNS Handling

### 5.1 DNS Leak Prevention

`StrictRoute=true` installs WFP filters blocking port-53 traffic on non-TUN interfaces.
Windows sends DNS to all interfaces simultaneously by default ("multihomed DNS") — WFP stops this.

### 5.2 DNS Hijacking (sing-box layer, not sing-tun)

sing-box configures a DNS server on the TUN's assigned DNS address.
In Options: `DNSServers: []netip.Addr{tunGatewayAddr}`.
sing-box's router sniffs UDP port 53, applies `hijack-dns` rule action, routes to internal DNS.

### 5.3 Fake-IP

sing-box DNS server returns fake IPs (198.18.0.0/16) for domain lookups.
When app connects to fake IP, TUN intercepts packet, system maps fake IP → original domain name
(from the DNS query that produced that fake IP), forwards with SNI intact.

### 5.4 DNS Flush on Close

`NativeTun.Close()` calls `FlushDNSResolverCache()` via Windows API so stale entries
(especially fake-IP entries) don't persist after the proxy shuts down.

---

## 6. Key Files and Roles

| File | Role |
|------|------|
| `tun.go` | Core interfaces: `Tun`, `WinTun`, `LinuxTUN`, `DarwinTUN`, `Stack`, `Handler`, `Options` |
| `tun_windows.go` | `NativeTun`: Wintun init, configure, read/write, WFP strict routing, rate juggler |
| `tun_windows_gvisor.go` | `WintunEndpoint`: bridges Wintun ↔ gVisor dispatcher |
| `stack.go` | `NewStack()` factory, `StackOptions`, stack selection logic |
| `stack_system.go` | System stack: NAT tables, packet rewriting, platform-specific read loops |
| `stack_gvisor.go` | gVisor stack init: endpoint creation, TCP/UDP/ICMP forwarder registration |
| `stack_gvisor_tcp.go` | `TCPForwarder`: gVisor TCP interception + loopback handling |
| `stack_mixed.go` | Mixed: TCP via system, UDP via gVisor, packet routing between them |
| `monitor_windows.go` | `NetworkUpdateMonitor`: route/interface change detection via Windows callbacks |
| `redirect_*.go` | Linux-only: iptables/nftables auto-redirect (not relevant to Windows) |
| `packages_*.go` | Android/Linux per-app filtering by UID/package name |
| `route_direct.go` | Direct route mapping for ICMP and bypass traffic |
| `gtcpip/` | Stripped-down gVisor tcpip package (only networking, no container runtime) |

---

## 7. Dependencies

| Dependency | Role |
|------------|------|
| `github.com/sagernet/gvisor` | Forked gVisor netstack (tcpip only, sing-box-mod patches) |
| `github.com/sagernet/sing` | Core sing abstractions (Handler, Logger, network types) |
| `golang.org/x/sys` | Windows syscalls, LUID ops, WFP bindings |
| `github.com/go-ole/go-ole` | COM/OLE for some Windows API calls |
| `github.com/sagernet/nftables` | Linux nftables auto-redirect |
| `github.com/florianl/go-nfqueue` | Linux NFQUEUE for packet interception |
| `github.com/sagernet/netlink` | Linux netlink for route/interface management |
| `go4.org/netipx` | Extended netip prefix/range operations |
| `golang.org/x/net` | Network utilities |

`golang.zx2c4.com/wintun` is an indirect dependency pulled in by the OS layer, not listed in go.mod
directly — the NativeTun uses it via CGO/DLL loading at runtime.

---

## 8. What Makes It Production-Grade

### vs. naive tun2socks implementation:

| Concern | Naive | sing-tun |
|---------|-------|----------|
| Windows TUN driver | self-compiled / WinDivert | Code-signed Wintun (NDIS, no test-signing needed) |
| DNS leak | None | WFP filters block port-53 on physical NICs |
| Route split | /0 default override (breaks VPN server route) | Two /1 routes preserve original gateway |
| Spinloop | Always block on event | Rate-aware: spin at >100 Mbps, sleep at idle |
| TCP loop | Copy every packet | Zero-copy `ReadPacket()` with ring buffer |
| Stack choice | One fixed stack | 3 options (system/gVisor/mixed) tunable per use case |
| Network changes | Static routes break on reconnect | Monitor rebinds sockets on interface change |
| Process traffic | All-or-nothing | UID/package/interface filters (Android/Linux) |
| gVisor loopback | Proxy traffic re-enters TUN (infinite loop) | `TCPForwarderWithLoopback` detects and reroutes |
| DNS flush | Stale entries after shutdown | `FlushDNSResolverCache()` on Close() |
| Multihomed DNS (Windows) | Leaks via physical NIC | WFP sublayer blocks at kernel level |

### Why gVisor fork vs upstream gVisor:

Upstream gVisor is a full container runtime (500k+ LOC). Only `pkg/tcpip` (the network stack)
is needed. The `sagernet/gvisor` fork strips everything else, applies performance patches
(gLazyConn for TCP, Apple-specific buffer tuning), and pins a known-good API surface.
This reduces binary size and build time while enabling sing-box-specific optimizations.

### Zero-copy architecture:

Wintun exposes packets as slices into its internal ring buffer. `ReadPacket()` returns these
slices directly with a release callback. The system stack can rewrite headers in-place before
reinserting into Wintun — no alloc, no copy on the hot path.

---

## 9. MTU Strategy

| Platform | MTU |
|----------|-----|
| iOS/macOS Network Extension | 4064 (NEPacketTunnelProvider limit) |
| Android | 9000 (jumbo frames, high throughput) |
| Windows/Linux/macOS desktop | 65535 (maximum, reduces fragmentation overhead) |

65535 MTU on desktop means packets are never fragmented inside the TUN. The outer connection
(Wintun → proxy) handles its own MTU negotiation with the physical network.

---

## Sources

- [SagerNet/sing-tun GitHub](https://github.com/SagerNet/sing-tun)
- [sing-box TUN configuration](https://sing-box.sagernet.org/configuration/inbound/tun/)
- [TUN Inbound Internals](https://singbox-internals.hidandelion.com/protocols/tun.html)
- [metacubex/sing-tun Go package docs](https://pkg.go.dev/github.com/metacubex/sing-tun)
- [Wintun official site](https://www.wintun.net/)
- [sing-box issue #2396 - stack options on Windows](https://github.com/SagerNet/sing-box/issues/2396)
- [SagerNet gVisor tun2socks discussion #142](https://github.com/SagerNet/SagerNet/discussions/142)
- [Windows virtual network with Wintun and tun2socks](https://awakecoding.com/posts/windows-virtual-network-using-wintun-and-tun2socks/)
