# tun2proxy Analysis

**Version analyzed:** 0.7.20 (latest as of 2026-04)
**Repo:** https://github.com/tun2proxy/tun2proxy
**License:** MIT
**Stars:** ~1,300 | **Forks:** 165 | **Releases:** 73

---

## Overview

tun2proxy is a Rust userspace tool that creates a TUN virtual network interface and routes all intercepted TCP/UDP traffic through an upstream SOCKS4/4a/5 or HTTP(S) proxy. No per-application configuration needed — it captures at the network layer.

**Supported proxy protocols:**
- SOCKS4 / SOCKS4a
- SOCKS5 (with username/password auth)
- HTTP CONNECT (basic auth + digest auth)

**Supported platforms:** Linux, macOS, iOS, Android, Windows

---

## TCP/IP Stack

**Not smoltcp.** tun2proxy uses the `ipstack` crate (by narrowlink) — a purpose-built async userspace TCP/IP stack for TUN devices.

### ipstack crate

- Crate: `ipstack` v1.x
- Repo: https://github.com/narrowlink/ipstack
- Description: "Asynchronous lightweight userspace implementation of TCP/IP stack for Tun device"
- Dependencies: `tokio`, `etherparse` (packet parsing)
- 97.87% doc coverage

**How ipstack works:**

1. `IpStack::new(device, config)` — wraps the async TUN device
2. Spawns background task reading raw frames from TUN via `device.read()`
3. On Unix: strips optional 4-byte packet-info header
4. Parses bytes into `NetworkPacket` via etherparse (extracts IP headers + transport layer)
5. Matches packet against existing sessions via `NetworkTuple` key `(src_addr, dst_addr, src_port, dst_port, proto)`
6. New session → creates `IpStackTcpStream` or `IpStackUdpStream` → queues to accept channel
7. Existing session → routes packet into that stream's bounded channel
8. Application calls `ip_stack.accept().await` → receives typed `IpStackStream` enum

```rust
// IpStackStream enum
enum IpStackStream {
    Tcp(IpStackTcpStream),   // implements AsyncRead + AsyncWrite
    Udp(IpStackUdpStream),
    UnknownTransport(IpStackUnknownTransport),  // ICMP, etc.
}
```

When app writes to `IpStackTcpStream`, `process_upstream_recv()` encodes bytes back to raw IP frames and writes to TUN device — completing the bidirectional relay.

---

## Packet Flow: TUN → SOCKS5

```
App on host OS
  │ TCP SYN to 1.2.3.4:443
  ▼
Routing table → TUN interface (tun0 / wintun)
  │ raw IP frame
  ▼
tun::Device (async read)
  │ Vec<u8> frame
  ▼
IpStack background task (ipstack crate)
  │ etherparse: parse IPv4/IPv6 + TCP/UDP headers
  │ lookup session by NetworkTuple
  │ new session → create IpStackTcpStream
  ▼
ip_stack.accept().await  [in run() loop]
  │ IpStackStream::Tcp(tcp_stream)
  ▼
SessionInfo { src, dst, protocol }
  │ dst IP may be virtual DNS IP → resolve to domain
  ▼
ProxyHandlerManager::new_handler(session_info)
  │ returns Box<dyn ProxyHandler>  (SocksConnection or HttpConnection)
  ▼
tokio::spawn: handle_tcp_session(tcp_stream, proxy_handler, args)
  │
  ├─ TcpStream::connect(proxy_server_addr).await
  │
  ├─ SOCKS5 state machine (socks.rs):
  │   ClientHello      → send [0x05, n_methods, methods...]
  │   ServerHello      ← read chosen method
  │   SendAuthData     → send [0x01, ulen, user, plen, pass]
  │   ReceiveAuthResp  ← read [0x01, 0x00] (success)
  │   SendRequest      → send [0x05, 0x01, 0x00, addr_type, addr, port]
  │   ReceiveResponse  ← read [0x05, 0x00, ...]
  │   Established      ↔ bidirectional relay
  │
  └─ copy_and_record_traffic(tcp_stream ↔ proxy_tcp_stream)
       8KB buffer, updates TrafficStatus counters
```

For UDP sessions: same accept loop, but `IpStackStream::Udp` → either UDP associate via SOCKS5 or UDPGW encapsulation.

---

## DNS Handling

Three modes, selected via `--dns` flag:

### 1. Virtual DNS (default, recommended)

- `--dns virtual` (default)
- Virtual pool: `198.18.0.0/15` (configurable via `--virtual-dns-pool`)
- Set system DNS to `198.18.0.1`

**Flow:**
1. DNS query arrives at TUN interface (UDP port 53 → virtual pool IP)
2. ipstack accepts as UDP stream
3. `VirtualDns::generate_query()`:
   - Parses query with `hickory-proto`
   - Allocates next IP from pool (sequential, wraps at pool end)
   - Stores mapping: `198.18.x.x → "example.com"` in LRU cache (60s TTL)
   - Returns synthetic DNS response pointing to allocated IP
4. When app connects to `198.18.x.x:443`:
   - `SessionInfo.dst` = virtual IP
   - `VirtualDns::reverse_lookup(ip)` → returns `"example.com"`
   - SOCKS5 `SendRequest` uses domain name, not IP → proxy resolves
5. `touch_ip()` refreshes TTL on active connections

**VirtualDns internals:**
- LRU cache: `hashlink` ordered map (IP → (domain, expiry))
- Reverse map: `HashMap<domain → IP>`
- IPv4+IPv6 both supported (separate CIDR pools)
- Expires old entries on lookup (60s timeout)

### 2. Over-TCP

- `--dns over-tcp`
- DNS queries forwarded over TCP to real resolver (DNS-over-TCP RFC)
- Works through SOCKS5 TCP stream
- Other UDP still dropped

### 3. Direct

- `--dns direct`
- DNS resolves locally (bypasses tunnel)
- Risk: DNS leaks real client IP

---

## Source Files and Roles

```
src/
├── lib.rs              Entry point, module declarations, run() main loop
├── general_api.rs      general_run_async() — TUN setup, IpStack init, calls run()
├── args.rs             Args struct (23 fields), ArgProxy, ArgDns enum
├── proxy_handler.rs    ProxyHandler trait + ProxyHandlerManager trait
├── socks.rs            SocksConnection — SOCKS4/4a/5 state machine
├── http.rs             HttpConnection — HTTP CONNECT state machine  
├── virtual_dns.rs      VirtualDns — LRU IP pool, DNS intercept/respond
├── dns.rs              DNS message parsing utils (hickory-proto wrappers)
├── session_info.rs     SessionInfo struct {src, dst, protocol, id}
├── directions.rs       IncomingDataEvent / OutgoingDataEvent type aliases
├── traffic_status.rs   TrafficStatus — bytes_tx, bytes_rx counters
├── no_proxy.rs         Bypass handler for direct connections
├── socket_transfer.rs  UNIX socket FD transfer (for namespace isolation)
├── udpgw.rs            UDP gateway — encapsulate UDP-in-TCP
├── win_svc.rs          Windows service integration (windows-service crate)
├── android.rs          JNI bindings for Android
└── directions.rs       DataEvent<D> with IncomingDirection/OutgoingDirection
```

### Key function: `run()` in lib.rs

```rust
pub async fn run(device, tun_mtu, args, shutdown_token) -> Result<usize>
```

- Creates `IpStack::new(device, ip_stack_config)`
- Creates `VirtualDns` if `args.dns == ArgDns::Virtual`
- Main accept loop:
  ```
  loop {
      select! {
          stream = ip_stack.accept() => match stream {
              IpStackStream::Tcp(s)  => spawn handle_tcp_session(s, ...)
              IpStackStream::Udp(s)  => spawn handle_udp_session(s, ...)
              IpStackStream::UnknownTransport(_) => drop
          }
          _ = shutdown_token.cancelled() => break
      }
  }
  ```

### Key function: `general_run_async()` in general_api.rs (lines 166-278)

- Parses `args.tun` name or `args.tun_fd` file descriptor
- Configures `tun::Configuration`:
  - address: `TUN_IPV4`, netmask: `TUN_NETMASK`, mtu: `tun_mtu`
  - Linux: enables packet_information flag
  - Windows: sets device GUID for wintun
- `let device = tun::create_as_async(&tun_config)?`
- Calls `crate::run(device, tun_mtu, args, shutdown_token).await`

---

## Windows Support

**Status: Functional but requires manual setup**

- TUN backend: **wintun** (Windows TUN driver)
- The `tun` crate uses wintun under the hood on Windows
- wintun DLL must be present at `%USERPROFILE%\.cargo\bin\wintun.dll`
- Requires Administrator privileges (always)
- `--setup` flag: auto-configures routing table via `netsh` commands
- Windows service: `windows-service` crate integration (`win_svc.rs`) — can run as system service
- Known issue: MTU setting via `netsh interface ipv4 set subinterface "wintun" mtu=65535` fails on some Windows 10 configs (issue #195, closed "not planned")

**Windows-specific dep:**
```toml
[target.'cfg(windows)'.dependencies]
windows-service = "0.8"
```

---

## Dependencies (Cargo.toml)

### Core runtime
| Crate | Version | Purpose |
|-------|---------|---------|
| `tokio` | 1.x (full) | Async runtime |
| `ipstack` | 1.x | Userspace TCP/IP stack for TUN |
| `tun` | 0.8 (async) | TUN device abstraction (Linux/macOS/Windows via wintun) |

### Proxy protocols
| Crate | Version | Purpose |
|-------|---------|---------|
| `socks5-impl` | 0.8 (tokio) | SOCKS5 protocol implementation |
| `httparse` | 1.x | HTTP response header parsing |
| `digest_auth` | 0.3 | HTTP Digest authentication |
| `base64easy` | 0.1 | HTTP Basic auth encoding |

### DNS
| Crate | Version | Purpose |
|-------|---------|---------|
| `hickory-proto` | 0.25 | DNS message parsing/building |
| `udp-stream` | 0.0.12 | UDP stream abstraction |

### Utilities
| Crate | Version | Purpose |
|-------|---------|---------|
| `clap` | 4 (derive) | CLI argument parsing |
| `serde` | 1 (derive) | Serialization |
| `tokio-util` | 0.7 | Codec, io utilities |
| `log` + `env_logger` | 0.4 / 0.11 | Logging |
| `hashlink` | — | LRU cache (virtual DNS) |
| `ipnet` / `ip_network` | — | CIDR operations |

### Platform-specific
| Crate | Platform | Purpose |
|-------|----------|---------|
| `nix` | Unix | Syscalls (fs, socket, uio features) |
| `daemonize` | Unix | Background daemon |
| `tproxy-config` | Unix | Transparent proxy routing config |
| `windows-service` | Windows | Win service management |
| `jni` | Android | JNI bindings |
| `android_logger` | Android | Platform logging |

### Optional
| Feature | Crate | Purpose |
|---------|-------|---------|
| `mimalloc` | `mimalloc` | Alternative memory allocator |

---

## Complexity Estimate

- **Source size:** 294.55 KB (crate source)
- **Total dep tree:** ~26-47 MB, 659K SLoC
- **Doc coverage:** 31.43% (33/105 items documented)
- **Build time:** ~2.5 minutes (full dep tree)
- **Active modules:** ~14 source files in `src/`
- **Lines in main crate:** estimate ~3,000-5,000 lines (294 KB source / ~60 bytes per line avg)
- **Complexity:** Medium. Core logic is clean event loop + state machines. Complexity in platform-specific TUN setup and proxy protocol edge cases (SOCKS4 IPv6 workarounds, digest auth state, UDPGW framing).

---

## Architecture Summary

```
┌──────────────────────────────────────────────────────┐
│                    Host OS                           │
│  App → TCP SYN → routing table → TUN interface       │
└────────────────────────┬─────────────────────────────┘
                         │ raw IP frames
                         ▼
              ┌─────────────────────┐
              │   tun crate         │  (wintun on Windows)
              │   AsyncRead/Write   │
              └──────────┬──────────┘
                         │
                         ▼
              ┌─────────────────────┐
              │   ipstack crate     │  userspace TCP/IP
              │   etherparse parse  │  NOT smoltcp
              │   session demux     │
              └──────────┬──────────┘
                         │ IpStackStream
                         ▼
              ┌─────────────────────┐
              │   run() accept loop │
              │   tokio::spawn per  │
              │   connection        │
              └──────────┬──────────┘
                         │
              ┌──────────┴──────────┐
              │                     │
         TCP session           UDP session
              │                     │
    ┌─────────▼──────┐    ┌─────────▼──────┐
    │ ProxyHandler   │    │ UDPGW or       │
    │ SocksConnection│    │ UDP-associate  │
    │ HttpConnection │    └────────────────┘
    └─────────┬──────┘
              │ TCP connect to proxy
              ▼
    ┌─────────────────────┐
    │  Upstream SOCKS5    │
    │  or HTTP proxy      │
    └─────────────────────┘
```

**Virtual DNS interception** happens before session dispatch: UDP port 53 packets to virtual pool IPs are answered locally, and the mapped domain name is injected into the SOCKS5 CONNECT request.

---

## Relevance to mirage-client-core

tun2proxy uses `ipstack` (not smoltcp) as its TCP/IP layer. The mirage-client-core already has a custom `tun/stack.rs` and `tun/nat_bridge/` — tun2proxy's architecture confirms:

1. **ipstack pattern** (accept loop + stream enum) is the dominant approach for userspace TUN proxying in Rust — consider it as drop-in vs custom stack
2. **Virtual DNS** at `198.18.0.0/15` is battle-tested; same pattern viable for mirage transparent mode
3. **ProxyHandler trait** (strategy pattern, state machine per protocol) is clean — directly applicable to mirage's SOCKS5 + onion routing handlers
4. **wintun** is the only viable TUN driver on Windows — no alternative
5. **No smoltcp** — confirms that smoltcp is overkill for this use case; ipstack is lighter and purpose-fit

---

## Sources

- [tun2proxy GitHub](https://github.com/tun2proxy/tun2proxy)
- [tun2proxy docs.rs](https://docs.rs/tun2proxy/0.7.20/tun2proxy/)
- [tun2proxy lib.rs source](https://docs.rs/tun2proxy/0.7.20/src/tun2proxy/lib.rs.html)
- [tun2proxy general_api.rs source](https://docs.rs/tun2proxy/0.7.20/src/tun2proxy/general_api.rs.html)
- [ipstack crate (narrowlink)](https://github.com/narrowlink/ipstack)
- [ipstack docs.rs](https://docs.rs/ipstack/latest/ipstack/)
- [tun2proxy Usage Wiki](https://github.com/tun2proxy/tun2proxy/wiki/Usage-(English))
- [UDP Gateway Wiki](https://github.com/tun2proxy/tun2proxy/wiki/UDP-gateway-feature)
- [socks5-impl crate](https://github.com/tun2proxy/socks5-impl)
- [lib.rs crate page](https://lib.rs/crates/tun2proxy)
