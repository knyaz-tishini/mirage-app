# nat_bridge Removal Plan

Switch from handwritten `nat_bridge` (userspace TCP/UDP packet mangling) to `mirage-ipstack`
(proper userspace IP stack, e.g. smoltcp-based).

---

## FILES TO DELETE (6 files — entire nat_bridge module)

```
mirage2web/crates/mirage-client-core/src/tun/nat_bridge/mod.rs
mirage2web/crates/mirage-client-core/src/tun/nat_bridge/checksum.rs
mirage2web/crates/mirage-client-core/src/tun/nat_bridge/dispatcher.rs
mirage2web/crates/mirage-client-core/src/tun/nat_bridge/socks5.rs
mirage2web/crates/mirage-client-core/src/tun/nat_bridge/tcp_handler.rs
mirage2web/crates/mirage-client-core/src/tun/nat_bridge/udp_handler.rs
```

And the desktop glue:

```
mirage2web/crates/mirage-client-desktop/src/tun/windows/nat_runner.rs
```

---

## FILES TO EDIT

### 1. `mirage-client-core/src/tun/mod.rs`

Remove line:
```rust
pub mod nat_bridge;
```

Result (3 lines remain):
```rust
pub mod config;
pub mod nat;
pub mod packet;
pub mod stack;
pub mod traits;
```

`pub mod nat;` — keep. nat.rs contains `FlowKey` struct used by dispatcher. Once
dispatcher is gone, check if FlowKey is still needed by mirage-ipstack; if not, remove
`pub mod nat;` too and delete `nat.rs`.

### 2. `mirage-client-core/src/tun/stack.rs`

Three additions were made for nat_bridge — each must be evaluated:

| Addition | Location | Keep or Revert? |
|---|---|---|
| `tun: Arc<Mutex<Box<dyn PlatformTun>>>` (instead of bare `Box`) | `TunStack` struct, line 35 | **KEEP** — mirage-ipstack will also need shared access to the TUN device |
| `extras: Vec<Box<dyn std::any::Any + Send>>` field | `TunStack` struct, line 40–41 | **KEEP** — generic drop-on-stop pattern, useful for ipstack handle too |
| `pub fn tun_device(&self) -> Arc<Mutex<Box<dyn PlatformTun>>>` method | lines 55–57 | **KEEP** — ipstack runner will call this same method |
| `pub fn add_extra(&mut self, extra: Box<dyn std::any::Any + Send>)` method | lines 60–62 | **KEEP** — ipstack handle stored via this same method |

Nothing in `stack.rs` needs reverting. The Arc<Mutex> wrapping and extras pattern are
correct abstractions that mirage-ipstack reuses.

The only comment to update:

- Line 39: `/// Platform-specific extras (e.g. Windows NAT runner). Dropped on stop().`
  → change to: `/// Platform-specific extras (e.g. Windows IP stack runner). Dropped on stop().`
- Line 55: doc comment `// for the NAT bridge runner`
  → change to `// for the IP stack runner`
- Line 157 comment in `stop()`: `// Drop extras first (e.g. NAT runner — cancels its tasks).`
  → `// Drop extras first (e.g. IP stack runner — cancels its tasks).`

### 3. `mirage-client-desktop/src/tun/windows/mod.rs`

Remove:
```rust
pub mod nat_runner;
// ...
pub use nat_runner::WintunNatRunner;
```

Add instead (once mirage-ipstack runner exists):
```rust
pub mod ipstack_runner;
// ...
pub use ipstack_runner::WintunIpStackRunner;
```

### 4. `mirage-client-desktop/src/tun_mode.rs` — Windows branch (lines 73–125)

Remove the entire nat_bridge block:
```rust
use mirage_client_core::tun::nat_bridge::NatBridgeConfig;
// ...
let bridge_config = NatBridgeConfig { ... };
let device = stack.tun_device();
let cancel = CancellationToken::new();
let runner = WintunNatRunner::start(device, bridge_config, cancel).await?;
stack.add_extra(Box::new(runner));
```

Replace with mirage-ipstack equivalent (structure is the same, type names change):
```rust
use crate::tun::windows::WintunIpStackRunner;

let device = stack.tun_device();
let cancel = CancellationToken::new();
let runner = WintunIpStackRunner::start(device, socks_port, cancel).await?;
stack.add_extra(Box::new(runner));
```

The `CancellationToken` import stays (it's needed for the new runner too).
The import `use mirage_client_core::tun::nat_bridge::NatBridgeConfig;` is removed.

### 5. `mirage-client-desktop/Cargo.toml`

No nat_bridge-specific deps. `tokio-util` is used by `main.rs` and other places —
do NOT remove it.

### 6. `mirage-client-core/Cargo.toml`

`tokio-util` is used by `src/management/handlers.rs` and `src/runtime.rs`
(CancellationToken) — do NOT remove it.

No nat_bridge-specific deps exist in either Cargo.toml. Nothing to change here.

---

## DEPENDENCY CHAIN (who imports what)

```
nat_bridge/mod.rs
  ← nat_bridge/dispatcher.rs  (imports NatBridgeState, tcp_handler, udp_handler)
  ← nat_bridge/tcp_handler.rs (imports NatBridgeState, TcpFlowEntry, checksum, socks5)
  ← nat_bridge/udp_handler.rs (imports NatBridgeState, checksum)
  ← nat_bridge/checksum.rs    (no internal imports — pure math)
  ← nat_bridge/socks5.rs      (no internal imports — pure SOCKS5 handshake)

nat_bridge (pub mod) imported by:
  ← mirage-client-core/src/tun/mod.rs  (pub mod nat_bridge)
  ← mirage-client-desktop/src/tun/windows/nat_runner.rs
        (use mirage_client_core::tun::nat_bridge::{start_nat_bridge, NatBridgeConfig, NatBridgeHandle})
  ← mirage-client-desktop/src/tun_mode.rs  (cfg windows)
        (use mirage_client_core::tun::nat_bridge::NatBridgeConfig)

nat_runner imported by:
  ← mirage-client-desktop/src/tun/windows/mod.rs  (pub mod nat_runner; pub use nat_runner::WintunNatRunner)
  ← mirage-client-desktop/src/tun_mode.rs  (use crate::tun::windows::WintunNatRunner)
```

All references are self-contained. No callers outside `mirage-client-core` and
`mirage-client-desktop`.

---

## NEW INTEGRATION POINT (mirage-ipstack replaces nat_bridge)

### What mirage-ipstack must provide

```rust
// crate: mirage-ipstack  (new)
pub struct IpStackRunner { ... }

impl IpStackRunner {
    pub async fn start(
        device: Arc<Mutex<Box<dyn PlatformTun>>>,
        socks5_port: u16,
        cancel: CancellationToken,
    ) -> Result<Self, MirageError>;
}

impl Drop for IpStackRunner {
    fn drop(&mut self) { self.cancel.cancel(); }
}
```

### Where it plugs in (desktop/src/tun_mode.rs, Windows branch)

```rust
#[cfg(target_os = "windows")]
pub async fn start_tun_mode(...) -> Result<TunStack, MirageError> {
    // ... same tun/firewall/routing setup ...
    let mut stack = TunStack::new(...);
    stack.start(relay_addrs, socks_port, exempt_ports).await?;

    // NEW: ipstack runner instead of nat_bridge
    let device = stack.tun_device();           // Arc<Mutex<Box<dyn PlatformTun>>>
    let cancel = CancellationToken::new();
    let runner = IpStackRunner::start(device, socks_port, cancel).await?;
    stack.add_extra(Box::new(runner));         // same pattern as before

    Ok(stack)
}
```

`TunStack::tun_device()`, `TunStack::add_extra()`, and the `extras` drop-on-stop
mechanism all remain unchanged — they are the stable integration contract.

---

## nat.rs / FlowKey

`crate::tun::nat::FlowKey` is currently used only inside `nat_bridge/`:
- `nat_bridge/mod.rs` imports it
- `nat_bridge/dispatcher.rs` builds `FlowKey` instances

Once nat_bridge is deleted, check if mirage-ipstack uses `FlowKey`:
- If yes → keep `pub mod nat;` in tun/mod.rs and keep nat.rs
- If no  → also delete `mirage-client-core/src/tun/nat.rs` and remove `pub mod nat;`

---

## Removal Order (step-by-step)

1. Delete 6 nat_bridge source files
2. Delete nat_runner.rs
3. Edit tun/mod.rs — remove `pub mod nat_bridge;`
4. Edit tun/windows/mod.rs — remove nat_runner module + re-export
5. Edit tun_mode.rs — remove Windows nat_bridge block, add ipstack block
6. Update 3 doc comments in stack.rs (NAT runner → IP stack runner)
7. Check if nat.rs / FlowKey still needed; delete if not
8. `cargo check --package mirage-client-core`
9. `cargo check --package mirage-client-desktop`
