# Leaf Proxy Framework — Technical Analysis

Source: https://github.com/eycorsican/leaf  
Version analyzed: 0.14.2 (Feb 2026), codebase master branch  
Language: Rust 99.4%, Edition 2021

---

## Summary

Leaf is a full-featured transparent proxy framework with TUN support across Linux, macOS, Windows, iOS, Android. It supports two userspace TCP/IP stacks (lwip via C bindings, smoltcp pure Rust), fake DNS, SOCKS5/Shadowsocks/VMess/VLESS/Trojan outbound, and async Tokio runtime throughout. Code quality is high; architecture is clean with clear module boundaries.

---

## TUN Implementation — Windows

### Driver

Windows TUN uses **WinTun** — Microsoft-signed kernel driver shipped as `wintun.dll`. Leaf delegates to the `tun` crate (v0.7) which depends on `wintun-bindings ^0.3`. The DLL must be present at runtime (copy to `C:\Windows\System32` or same dir as exe). No NDIS filter, no TAP-Windows — pure WinTun ring buffer API.

### Setup sequence (Windows-specific in `inbound.rs`)

```rust
#[cfg(target_os = "windows")]
cfg.platform_config(|x| {
    x.device_guid(rng.gen());          // random GUID per session
    if !dns_servers.is_empty() {
        x.dns_servers(&dns_servers);   // inject DNS into TUN adapter
    }
    if let Some(f) = &settings.wintun {
        x.wintun_file(f.clone());      // optional custom wintun.dll path
    }
});
```

Before TUN creation, `OUTBOUND_INTERFACE` env var is set to the default interface IP (via `winsys.rs` / `ipconfig` crate). This makes outbound connections from the proxy itself bind to the real NIC, breaking the routing loop.

Interface metric is set to 0 so the TUN adapter wins route selection over the real NIC (default route goes through TUN).

### `winsys.rs`

Provides `get_default_interface_ips()` — enumerates adapters via `ipconfig` crate, finds the one with active gateways, returns IPv4/IPv6 as comma-separated string. Used to populate `OUTBOUND_INTERFACE` before the TUN device starts.

---

## Userspace TCP/IP Stack

Two stacks, selected at compile time via Cargo features. Both are used simultaneously — the `inbound-tun` feature enables both:

### netstack-lwip (C bindings)

- Repo: https://github.com/eycorsican/netstack-lwip
- Composition: 89.6% C, 10.2% Rust
- Pinned at git rev `4cc3162`
- Wraps lwIP (lightweight IP stack originally for embedded systems) with Rust-friendly async API
- Exports: `NetStack`, `TcpListener`, `TcpStream`, `UdpSocket`
- Global mutex (`LWIP_MUTEX`) serializes lwIP access (not thread-safe internally)
- Battle-tested stack, handles fragmentation/reassembly/retransmit correctly

### netstack-smoltcp (pure Rust)

- Repo: https://github.com/eycorsican/netstack-smoltcp  
- Branch: `pr-16-initialize-unfilled`
- Pure Rust, no C, built on `smoltcp` crate
- `StackBuilder` pattern: `.enable_tcp(true).enable_udp(true).enable_icmp(true).build()`
- Returns `(stack, runner, udp_socket, tcp_listener)`
- Supports packet filtering by src/dst IP
- Implements `tokio::io::{AsyncRead, AsyncWrite}` and `futures::{Stream, Sink}`
- Platform support: Linux, macOS, Windows, iOS, Android

**Which is default?** Both are compiled when `inbound-tun` feature is active. The runtime config (protobuf `TunInboundSettings`) selects which stack handles a given TUN inbound. Code in `inbound.rs` has parallel `new_lwip` / `new_smoltcp` paths with `#[cfg(feature)]` guards.

---

## Packet Flow: TUN → Stack → Proxy → Splice

```
[App traffic]
     │
     ▼
[WinTun ring buffer]   ← kernel driver reads/writes raw IP packets
     │
     ▼  (tun crate, async framed stream)
[TUN inbound handler]  ← leaf/src/proxy/tun/inbound.rs
     │
     ├──▶ Write IP packets into netstack (lwip or smoltcp)
     │
     │    Inside netstack:
     │    - Reassemble TCP streams from IP packets
     │    - Maintain TCP state machine
     │    - Expose TcpListener / UdpSocket to Rust
     │
     ├──▶ TCP: TcpListener.accept() → new TcpStream
     │         handle_inbound_stream_*() spawns tokio task
     │         Session{src, dst, inbound_tag} created
     │         Fake DNS check: if dst IP is fake → resolve to domain
     │         dispatcher.dispatch_stream(session, stream) called
     │
     └──▶ UDP: UdpSocket → NatManager
               Port 53 intercepted → fake DNS response generated locally
               Other UDP → NAT table → forwarded via outbound

[Dispatcher]  ← leaf/src/app/dispatcher.rs
     │   routes by rule set (IP/GeoIP/domain)
     │   optional TLS/HTTP domain sniffing
     ▼
[OutboundManager]
     │   selects handler: SOCKS5, Shadowsocks, Trojan, VMess, VLESS, Direct…
     ▼
[Outbound TCP connect]
     │   bidirectional copy: netstack stream ↔ proxy stream
     ▼
[Remote server / direct target]
```

Bidirectional data splice uses `tokio::io::copy_bidirectional` or equivalent async copy loops.

---

## DNS Handling

### Fake DNS (primary mechanism for TUN mode)

File: `leaf/src/app/fake_dns.rs`

- Reserved pool: `198.18.0.0/15` (198.18.0.0 – 198.18.255.255, ~65k addresses)
- Cursor allocates IPs sequentially, wraps around, evicts old mappings
- Two hash maps: `domain_to_ip` and `ip_to_domain` (reverse lookup)
- TTL configurable
- Thread-safe via `RwLock`

**Flow:**
1. App does DNS query for `example.com`
2. TUN intercepts UDP port 53
3. FakeDNS generates response: `example.com → 198.18.0.42`
4. App connects to `198.18.0.42:443`
5. TUN inbound sees TCP to `198.18.0.42`
6. ip_to_domain lookup → `example.com`
7. Session.destination = `SocksAddr::Domain("example.com", 443)`
8. Dispatcher routes by domain, not IP

**Modes:**
- Include mode: only whitelisted domains get fake IPs (rest go direct)
- Exclude mode: all domains get fake IPs except blacklist
- Mutually exclusive — code asserts at startup

### DNS forwarding

Windows-specific: `dns_servers` list injected directly into the WinTun adapter config via `x.dns_servers()`. This forces OS DNS traffic through the TUN, ensuring fake DNS intercepts all queries.

---

## Key Source Files

| File | Role |
|------|------|
| `leaf/src/proxy/tun/inbound.rs` | Main TUN handler: stack init, TCP/UDP dispatch, fake DNS integration, Windows wintun config |
| `leaf/src/proxy/tun/mod.rs` | Module: declares `inbound`, re-exports netstack crates |
| `leaf/src/app/fake_dns.rs` | Fake DNS IP pool, query intercept, response synthesis |
| `leaf/src/app/dispatcher.rs` | Core router: session → outbound handler mapping, domain sniffing |
| `leaf/src/winsys.rs` | Windows: `get_default_interface_ips()` via `ipconfig` crate |
| `leaf/src/session.rs` | `Session` struct, `SocksAddr` (IP or domain), `Network` enum |

External:
| Repo | Role |
|------|------|
| `eycorsican/netstack-lwip` | lwIP C bindings, `NetStack` / `TcpListener` / `UdpSocket` |
| `eycorsican/netstack-smoltcp` | Pure Rust smoltcp wrapper, same API surface |
| `meh/rust-tun` (v0.7) | Cross-platform TUN device, WinTun on Windows |

---

## Cargo.toml — TUN Dependencies

```toml
[dependencies]
tun = { version = "0.7", features = ["async"], optional = true }
netstack-lwip = { git = "https://github.com/eycorsican/netstack-lwip", rev = "4cc3162", optional = true }
netstack-smoltcp = { git = "https://github.com/eycorsican/netstack-smoltcp", branch = "pr-16-initialize-unfilled", optional = true }
pnet_datalink = { version = "0.34", optional = true }   # macOS + Linux only
ipconfig = "0.3"   # Windows: enumerate adapters

[features]
inbound-tun = ["tun", "netstack-lwip", "netstack-smoltcp", "pnet_datalink"]
```

Other key deps: `tokio 1.x` (full), `protobuf 3.6`, `rand 0.8`, `bytes 1.x`, `thiserror`, `async-trait`, `socket2 0.5`

TLS backends (choose one at compile time):
- `aws-lc-rs 1.16` + `rustls` + `tokio-rustls 0.26` (default)
- `ring 0.17` + `rustls`
- `openssl 0.10` (vendored) + `tokio-openssl`

QUIC via `quinn 0.11`. WebSocket via `tungstenite 0.24` + `tokio-tungstenite`.

---

## Windows Support Status

**Status: Working but requires manual setup.**

- WinTun driver (wintun.dll) required — must be pre-installed or bundled
- Must run as Administrator (WinTun kernel driver installation)
- Random GUID assigned per session — creates new TUN adapter each run
- DNS servers injected into TUN adapter config
- `OUTBOUND_INTERFACE` env var must be set before TUN creation to avoid routing loop
- Interface metric 0 required for default route override

**History:**
- PR #363 "wintun support" was closed as abandoned (April 2024) — separate PR
- Issue #268 "windows transparent proxy" marked completed Feb 2026
- Community has working setups as of early 2025

**Maple** (https://github.com/YtFlow/Maple) is a UWP wrapper around leaf-ffi for Windows, using the UWP VPN Platform instead of direct WinTun — avoids admin requirement but adds UWP complexity.

---

## Complexity Assessment

**Codebase size:** ~15k lines Rust (leaf crate), plus C in netstack-lwip

**Strengths:**
- Clean trait-based outbound abstraction (`InboundHandler`, `OutboundHandler`)
- Feature flags cleanly separate protocols — binary can be small
- Both pure-Rust (smoltcp) and C (lwip) stacks available
- Fake DNS design is elegant — avoids any DNS leak by construction
- Active maintenance (0.14.2 released Feb 2026)

**Weaknesses / Complexity sources:**
- `inbound.rs` is large: duplicated `_lwip` / `_smoltcp` variants for every handler function
- netstack-lwip is 90% C with Rust glue — cross-compilation to Windows can be painful
- netstack-smoltcp pinned to a non-merged branch (`pr-16-initialize-unfilled`) — upstream instability risk
- WinTun setup requires admin + DLL bundling — non-trivial for desktop app distribution
- Protobuf config schema adds complexity (requires `.proto` compilation step)
- No routing loop protection beyond `OUTBOUND_INTERFACE` env var — fragile if misconfigured

**Overall:** Production-grade, well-structured, actively maintained. The Windows TUN path works but involves more moving parts (wintun.dll, admin rights, metric config, env var) compared to Linux/macOS. Embeddability via `leaf-ffi` is clean.

---

## Sources

- https://github.com/eycorsican/leaf
- https://github.com/eycorsican/netstack-lwip
- https://github.com/eycorsican/netstack-smoltcp
- https://github.com/meh/rust-tun
- https://github.com/eycorsican/leaf/blob/master/leaf/src/proxy/tun/inbound.rs
- https://github.com/eycorsican/leaf/discussions/343
- https://github.com/YtFlow/Maple
- https://crates.io/crates/tun/0.7.18/dependencies
- https://crates.io/crates/wintun-bindings
