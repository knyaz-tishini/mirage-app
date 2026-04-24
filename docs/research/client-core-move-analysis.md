# mirage-client-core Dependency Analysis

Date: 2026-04-24

## 1. What mirage-client-core depends on

**Path:** `mirage2web/crates/mirage-client-core/Cargo.toml`

### Workspace-internal path deps (all from mirage2web):
| Dep | Feature flag |
|-----|-------------|
| `mirage-core` | — |
| `mirage-mux` | `tokio-compat` |
| `mirage-protocol` | — |

### External crates:
`futures`, `tokio`, `tracing`, `tracing-subscriber`, `thiserror`, `async-trait`, `serde`, `serde_json`, `reqwest`, `rand`, `axum`, `arc-swap`, `tokio-util`, `bytes`

### Source modules exposed (lib.rs):
`log_capture`, `monitor_link`, `tun_bridge`, `cover_tunnel`, `onion_tunnel`, `discovery`, `dns_forwarder`, `endpoint_protocol`, `http_proxy`, `management`, `mux_pool`, `pool`, `relay`, `runtime`, `shaped_tunnel`, `socks5`, `transparent`, `tunnel`, `udp_framing`

---

## 2. What depends on mirage-client-core inside mirage2web

References found via grep across mirage2web:

| File | How referenced |
|------|---------------|
| `mirage2web/Cargo.toml` | `[workspace.dependencies]` — `mirage-client-core = { path = "crates/mirage-client-core" }` |
| `mirage2web/crates/mirage-client-desktop/Cargo.toml` | `mirage-client-core = { path = "../mirage-client-core" }` (direct path, not workspace alias) |
| `mirage2web/crates/mirage-client-mobile/Cargo.toml` | `mirage-client-core = { path = "../mirage-client-core" }` (direct path) |
| `mirage2web/crates/mirage-client-mobile/src/lib.rs` | `use mirage_client_core` (indirect, comment-only architecture diagram) |

**mirage-node does NOT depend on mirage-client-core** — confirmed by grep.

---

## 3. What depends on mirage-client-core inside mirage2app

| File | How referenced |
|------|---------------|
| `mirage2app/crates/cli/Cargo.toml` | `mirage-client-core = { path = "../../../mirage2web/crates/mirage-client-core" }` |

**Other mirage2app crates do NOT depend on mirage-client-core:**
- `crates/desktop/windows/v1/Cargo.toml` (mirage2app binary) — only `mirage-core`
- `crates/desktop/windows/v2/Cargo.toml` (mirage2light binary) — only `mirage-core`
- `crates/mobile/Cargo.toml` — only `mirage-core`, `mirage-protocol`

---

## 4. What mirage2app depends on from mirage2web (path deps)

### Root workspace `mirage2app/Cargo.toml`:
```toml
mirage-core     = { path = "../mirage2web/crates/mirage-core" }
mirage-protocol = { path = "../mirage2web/crates/mirage-protocol" }
```
These are workspace-level declarations. Individual crates may override with direct paths.

### Per-crate breakdown:
| Crate | mirage2web deps |
|-------|----------------|
| `crates/cli` | `mirage-core` (workspace), `mirage-protocol` (workspace), **`mirage-client-core`** (direct path `../../../mirage2web/crates/mirage-client-core`) |
| `crates/desktop/windows/v1` | `mirage-core` (direct `../../../../../mirage2web/crates/mirage-core`) |
| `crates/desktop/windows/v2` | `mirage-core` (direct `../../../../../mirage2web/crates/mirage-core`) |
| `crates/mobile` | `mirage-core` (workspace), `mirage-protocol` (workspace) |

---

## 5. mirage2web workspace members

```
crates/mirage-core
crates/mirage-protocol
crates/mirage-node
crates/mirage-client-core
crates/mirage-client-desktop
crates/mirage-client-mobile
crates/mirage-mux
```

Total: 7 members.

---

## 6. mirage2app workspace members

```
crates/cli
crates/desktop/windows/v1
crates/desktop/windows/v2
# crates/mobile  ← commented out (cdylib, skipped on Windows build)
```

Total: 3 active members (mobile excluded by comment).

---

## 7. mirage-client-desktop and mirage-client-mobile: active or stale?

### mirage-client-desktop
**Active.** `src/main.rs` is a 520-line fully-implemented binary:
- Parses `ClientConfig`, builds protocol chains, creates `TunnelPool` per chain
- Spawns SOCKS5, HTTP CONNECT proxy, DNS forwarder, management server
- Implements kill switch, monitor link, mux pool, TUN bridge
- Uses virtually every module from `mirage-client-core`

`src/killswitch.rs` — also exists (Windows WFP kill switch implementation).

### mirage-client-mobile
**Active skeleton.** `src/lib.rs` is 32 lines — architecture comment + `cfg`-gated module declarations:
- `mod common` — only compiled on Android/iOS
- `mod android` — JNI bridge (Android only)
- `mod ios` — C FFI stubs (iOS only)
- `mod tun` — always compiled (has full Android+iOS sub-tree)

All platform-specific code is gated behind `cfg(target_os)`, so it compiles cleanly on Windows/dev but is not functional without a mobile target. NOT stale — it's the intended mobile FFI layer.

---

## Summary: Dependency Graph

```
mirage2web workspace:
  mirage-core
  mirage-protocol
  mirage-mux
      └── mirage-client-core   (depends on core + protocol + mux)
              ├── mirage-client-desktop  (bin: mirage-client) — ACTIVE
              └── mirage-client-mobile   (cdylib/staticlib) — ACTIVE skeleton

mirage2app workspace:
  crates/cli  ─────────────────────────────► mirage-client-core (cross-workspace path)
                                              mirage-core (via workspace)
                                              mirage-protocol (via workspace)
  crates/desktop/windows/v1 (mirage2app)  ──► mirage-core only
  crates/desktop/windows/v2 (mirage2light)──► mirage-core only
  crates/mobile (disabled)                ──► mirage-core + mirage-protocol
```

## Key findings for "client-core-move" planning

1. **mirage-client-core lives in mirage2web** but is consumed cross-workspace by `mirage2app/crates/cli` via a hardcoded relative path `../../../mirage2web/crates/mirage-client-core`.

2. **Only `crates/cli` in mirage2app uses it.** The GUI binaries (v1, v2) do not — they only need `mirage-core`.

3. **mirage-client-core's own deps** are all within mirage2web (`mirage-core`, `mirage-protocol`, `mirage-mux`). Moving it to mirage2app would mean those 3 crates must either also move or remain as cross-workspace path deps.

4. **mirage-client-desktop (in mirage2web) is a fully active, heavy user** of mirage-client-core — identical in scope to `mirage2app/crates/cli`. Both are essentially the same binary with minor differences. Moving client-core out of mirage2web would break mirage-client-desktop unless it also migrates or gets a cross-workspace path dep back.

5. **mirage-client-mobile is not stale** — it's an active (if platform-gated) FFI wrapper around client-core. Same migration concern applies.

6. **No circular deps** — mirage-node does not depend on mirage-client-core at all.
