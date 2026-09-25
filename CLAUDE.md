# mirage2app — agent rules

## Phase 8 ephemeral client identity (May 2026 clean-room rewrite)

См. `nemo/docs/plans/phase-8-report.md` + `phase-9-plan.md`.

After Phase 5+8 — **`ClientConfig.private_key` field is DELETED**. Client
generates a fresh ephemeral X25519 keypair per dial in
`mirage-protocol::handshake_client_stream` (`StaticSecret::random_from_rng(OsRng)`).
No long-term client identity stored on disk.

`ClientConfig.mesh_id: String` is **mandatory** (default `"staging-v2"`
via `#[serde(default)]`). Baked into Blake3 confirm — client with wrong
mesh_id cryptographically fails handshake. Used in `EndpointProtocol::new(..., mesh_id)` plumbed through `TunnelPool::new(mesh_id, peer_table)` / `MuxPool::new(mesh_id, peer_table)`.

### Single ProtocolChain path (Phase 5)

`build_chain_protocol_chain` no longer has alias-arms (`MirageTls` / `MirageWebsocket` / `Xhttp`). Single path:

```rust
let endpoint = MirageEndpointConfig {
    transport: chain.protocol.transport(),
    auth: AuthMode::Blake3,
    camouflage: chain.hops[0].camouflage.unwrap_or(None),
    byte_cipher: None,
    byte_cipher_mode: chain.hops[0].byte_cipher.clone(),
};
EndpointProtocol::new(endpoint, /* ephemeral keypair generated in driver */,
    chain.protocol, chain.mesh_id.as_bytes().to_vec())
```

`TcpProtocolId::Mirage` / `UdpProtocolId::MirageUdp` are the only variants.

### peer_table runtime resolution (Phase 8.4/8.5)

`HopConfig.public_key: Option<String>` — if `None`, resolve via live
`Arc<ArcSwap<PeerTable>>` shared between `MonitorLink` and `TunnelPool` /
`MuxPool`. MonitorLink парсит peer_table из WS Config frame, stores в
ArcSwap. Pool loads snapshot on each dial.

`HopConfig.node_id: Option<String>` — lookup key (prefer over `display_name`).

`ClientConfig` schema:
```json
{
  "client_id": "...",
  "mesh_id": "staging-v2",
  "chains": [{
    "chain_id": "...",
    "hops": [{
      "addr": "ip:port",
      "public_key": null,    // resolved via peer_table at dial
      "node_id": "v2-entry", // lookup key
      "camouflage": {...},   // per-leg outer wrap
      "byte_cipher": null
    }],
    "protocol": "Mirage",
    ...
  }],
  "routing": {...}
}
```

NO `private_key`, NO `l1`, NO `l2`, NO `l1_transparent`, NO `udp_quic` / `udp_hysteria2` / `udp_wg` / `udp_http3`, NO `mirage_websocket` / `mirage_xhttp` blocks — все эти поля удалены.

### Offline/no-monitor-link path

If client runs without monitor link (debug, isolated test), construct
`Arc::new(ArcSwap::from_pointee(PeerTable::default()))` — empty table.
Only chains with hardcoded `hop.public_key = Some(hex)` work; chains
relying on dynamic resolution fail at first dial with `UnknownNode` /
`BadPubkey`.

## Stable `sentinel_id` flow (l0-sentinel)

1. First boot: `ensure_sentinel_id(state_dir)` reads `${state_dir}/sentinel-id.txt`; if missing, generates UUID v4 and writes atomically (tmp + rename). On subsequent boots the same id is reused.
2. Register on every reachable LM in parallel, sending `{sentinel_id, role, shortname, os, version, supported_transports}` on `POST /relay/clients`. Monitor accepts the caller-supplied `sentinel_id` verbatim as `client_id` — same physical box ends up with the same `client_id` across all 9 LMs.
3. State directory:
   - Linux/root: `/root/.local/share/mirage/`
   - macOS `dev`: `/Users/dev/.local/share/mirage/`
   - Windows: `%LOCALAPPDATA%/mirage/state/`

The deprecated client-side `mirage-sentry` crate (in `crates/_deprecated/`) and its slavyanka reverse-SSH worker were removed 2026-05-24. Operator-side remote-shell access on client boxes now goes through `mirage-mshd` (operator-issued binary, separate per user — see `mirage2mshd/CLAUDE.md`).

## Naming (canonical)

In this workspace `sentinel` always means the client-side L0 agent (`mirage-l0-sentinel`). It is NOT the same as:

- `sentry` — server-side system scanner in `mirage2monitor/crates/sentry/`. Never lives on client boxes.
- `mshd` — operator-issued mesh-shell daemon (`mirage2mshd`). May land on a client box, but identity + bootstrap come from `POST /mshd/issue` on operator-box at issuance time — sentinel and mshd never share state or id.

Type `mirage_core::ids::SentinelId` covers (1) only. No `SentryId` type exists on the client side.

## Debug-shell + dynamic monitor URLs (Apr 2026; gated Sep 2026)

New crate `crates/debug-shell/` (`mirage-debug-shell`) — operator-triggered reverse-SSH tunnel. Sentinel reacts to two new WS frames from monitor (`monitor_link::WsServerFrame::DebugTunnelOpen { ssh_priv_key_pem, monitor_ssh_host, reverse_port, ttl_secs, ... }` and `DebugTunnelClose`); on Open it writes `${state_dir}/debug-tunnel.json` and spawns `mirage-debug-shell --config <path>`, on Close it SIGTERMs the child. `sentinel.toml` gains optional `debug_shell_binary` and `state_dir` fields. Same `WsServerFrame::Config` carries a new `peer_lm_urls: Vec<String>` field — sentinel persists it atomically to `${state_dir}/monitor_urls.json` and bootstraps from that file on next start (preferring it over static `monitor_urls`); on every change while running it calls `MonitorLink::reconfigure(...)` to swap the internal URL list. WS frames use the tagged enum `{kind: "config" | "debug_tunnel_open" | "debug_tunnel_close"}` — old monitors that emit untagged config still work via deserializer fallback. WS subscribe URL now carries `?client_id=<id>` query so monitor can route per-client pushes.

**Gated off by default (Sep 2026)** — a customer's own machine must never be
remotely shellable by us. `MonitorLink` (`client-core/src/monitor_link.rs`)
takes `allow_operator_debug_shell: bool` at construction (from
`sentinel.toml#allow_operator_debug_shell`, `false` by default) and refuses
every `DebugTunnelOpen`/`DebugTunnelClose` frame — on both the Wire and the
OverChain transport — before it ever reaches `frame_tx`, logging a `warn!`
once per monitor identity (`MonitorLink::admit_debug_tunnel_frame`). A
refused frame never reaches l0-sentinel's `spawn_frame_watcher`, so
`mirage-debug-shell` is never spawned regardless of whether
`debug_shell_binary` is configured. Nothing pushed by the monitor, and no
`AccessPacket` field, can turn this on — it is a purely local, hand-edited
`sentinel.toml` opt-in. A mode-3/customer issuance of `sentinel.toml` never
sets it.

## peer_lm_urls bootstrap order (sentinel)

Sentinel selects its initial monitor URL list with this precedence on every cold start:

1. **`${state_dir}/monitor_urls.json`** — last-known-good list, written atomically on each successful `WsServerFrame::Config` containing `peer_lm_urls`. Preferred when present and non-empty.
2. **`sentinel.toml#monitor_urls`** — static bootstrap fallback. Used only if the persisted file is missing or empty.

Once running, every config push that changes `peer_lm_urls` triggers `MonitorLink::reconfigure(...)` (live URL-list swap, no restart) and an atomic rewrite of `monitor_urls.json`. The list **does not shrink reactively** for already-connected legs — it's additive at runtime; restart sentinel to drop a retired URL.

## sentinel.toml fields (current)

```toml
monitor_url        = "https://lm-nl.zen-geldmaschine.net:19443"
monitor_urls       = [                                  # bootstrap fallback only
  "https://lm-nl.zen-geldmaschine.net:19443",
  "https://lm-nl-2.zen-geldmaschine.net:19443",
]
monitor_api_key    = "<Client role key>"
client_config_path = ".../client-config-managed.json"
client_binary      = ".../mirage-client-cli.exe"
client_args        = ["--config", "{client_config_path}"]
client_template    = ".../client-template.json"
poll_interval_secs = 30
respect_user_config = true

# debug-shell + state (May 2026)
state_dir          = "C:/ProgramData/mirage/state"     # required
debug_shell_binary = "C:/Program Files/Mirage/mirage-probe.exe"  # optional override
allow_operator_debug_shell = false  # default; local-only opt-in, see above
```

`state_dir` is now **required** — sentinel writes:

- `${state_dir}/monitor_urls.json` — persisted peer-LM list (atomic tmp+rename).
- `${state_dir}/debug-tunnel.json` — current `DebugTunnelOpen` config consumed by the spawned probe.

`debug_shell_binary` defaults to `mirage-probe` on `$PATH` if absent.

## L0 stack

mirage2app содержит **headless backend клиента**. Три процесса работают в связке (на десктопе и в будущем — мобиле):

| Crate | Роль |
|-------|------|
| `crates/l0-orchestrator/` (`mirage-l0-orchestrator`) | Топ-уровневый supervisor для desktop+mobile. Process mode (desktop) спавнит компоненты бинарями; task mode (mobile) — встраивает их через `lib::run()` в один процесс. См. `orchestrator.example.toml` |
| `crates/l0-sentinel/` (`mirage-l0-sentinel`) | Bootstrap: связь с монитором, материализация client-config, спавн cli. **Двойной крейт (lib + bin)** — `pub async fn run(SentinelConfig)` для embedded use из orchestrator |
| `crates/cli2/` (`supervisor` + `worker`) | **Единственный data-plane.** Split-arch: один supervisor + N worker-процессов (по чейну) с reattach через named pipes — soft-restart на OTA. Supervisor читает sentinel-овский `ClientConfig` JSON через `Config::load_any`/`from_client_config` |
| ~~`crates/cli/` (`mirage-client-cli`)~~ | 🔴 **RETIRED 2026-06-14** → `crates/_deprecated/cli/`. Legacy single-process data-plane (cli1). Полностью заменён cli2. НЕ использовать, НЕ собирать. Убран из workspace members, orchestrator dep, build-win-bins.sh, installer |

mirage2tun (отдельный submodule) — низкоуровневый TUN-перехватчик. Получает chains через `cli` POST. **Нельзя ходить в монитор напрямую из cli или tun** — только через sentinel. MonitorLink в самих cli/tun допустим только как fallback на случай мёртвого sentinel.

GUI потом натянется на этот headless backend как отдельная обёртка.

## subscriber-client config (edge session)

`crates/subscriber-client/` (`mirage-subscriber-client`) is the client for
modes 1 (public edge) and 2 (private/dedicated edge) — see
`docs/mirage/plans/2026-09-24-edge-as-entry.md` PART TWO. It is NOT cli2:
it knows only its own edge (the edge's faces + the edge's static key), never
a peer table, never a hop list. It holds ONE persistent mux session to the
edge (`EdgeHello`/`EdgeWelcome`/`StreamOpen`, over
`mirage2web/crates/mirage-dial-core/src/edge_session.rs`'s wire types +
`mirage-mux`), never one V9 tunnel per flow.

### Delivery artefact = the whole config shape

The operator hands the customer one JSON file — `EntranceArtefact`
(`crates/subscriber-client/src/artefact.rs`), unchanged by the edge-session
rewrite:

```json
{
  "v": 1,
  "private_key_hex": "<64 hex chars — the CUSTOMER's own persistent X25519 secret>",
  "mesh_id": "MESH2",
  "entrances": [
    {
      "addr": "172.86.76.34:443",
      "server_pubkey_hex": "<64 hex chars — the EDGE's own static X25519 pubkey>",
      "camouflage": { "type": "tls_real", "sni": "entrance.example.com", "alpn": ["h2"] }
    }
  ],
  "mode": "full_auto",
  "label": "pilot-1",
  "issued_at_ms": 1726000000000
}
```

- `entrances` — every face THIS edge publishes (host:port + camouflage).
  Several boxes are several independent entrances in principle, but for one
  edge every entry normally carries the SAME `server_pubkey_hex` — the
  edge's own operator-signed identity key, never a node key (PART ONE's
  perimeter/capsule fix). Plural only for face-sharding/failover, not for
  reaching more than one edge.
- `private_key_hex` — the CUSTOMER's own persistent static key, presented on
  every dial (`crate::dial::StaticKeyProtocol`, unchanged).
- `mode` — the default exit preference: `"full_auto"` or
  `{"type":"country","code":"NL"}`-shaped (see `ArtefactMode`). Converted to
  the wire's `ExitPreference` by `exit_preference_from_mode` and sent inside
  `EdgeHello` on every dial — the edge answers with the countries it can
  serve and the exit it actually fulfils (`EdgeWelcome`), never a node list.
- A UDP-shaped entrance (`TransportCamouflage::is_udp()`) is recognised but
  skipped with a logged warning — the edge-session dial only speaks
  TCP-shaped faces today (`crate::assembly`).

### Session + local surface

- `crate::session::EdgeSession::connect` — one V9 dial
  (`StaticKeyProtocol`) + `EdgeHello`/`EdgeWelcome` + mux upgrade
  (`mirage_mux::tokio_compat::SessionHandle`). Refuses BY NAME
  (`EdgeDialError::Refused`) on `EdgeOutcome::Refused`, never a bare IO
  error.
- `crate::session::EdgeSessionSupervisor` — holds the session up across the
  artefact's entrances (round-robin failover — the face sharding this crate
  already had, now applied to picking a dial path for the one persistent
  session rather than to sharding independent flows) and reconnects with
  exponential backoff (1s → 30s cap) whenever the live session closes.
- `crate::socks5` — a purpose-built SOCKS5 (RFC 1928) listener on
  `127.0.0.1:<port>` (default **1080**, `--bind` / `SubscriberClientOptions
  ::socks5_bind`). CONNECT opens one mux stream per flow
  (`EdgeSession::open_tcp_stream`, `StreamOpen{Tcp, host:port}` as the
  stream's first bytes — a domain name rides through UNRESOLVED so DNS
  resolves at the exit). NOT reused from `mirage_client_core::socks5::
  Socks5Server` (that type is `Router`/`ChainSet`/`TunnelPool`-shaped — a
  fixed pre-built chain set this crate no longer has).
- **UDP ASSOCIATE** (`crate::udp_associate`, `docs/mirage/plans/2026-09-25-edge-udp.md`
  §4) is served for real — a full RFC 1928 §7 relay, not a refusal. On
  `CMD_UDP_ASSOCIATE`, `serve_udp_association` binds a fresh loopback UDP
  socket and replies SUCCESS with its own `BND.ADDR`/`BND.PORT` on the
  control TCP stream. Every subsequent datagram the client sends there
  carries its own SOCKS5 UDP header (`[2B RSV][1B FRAG][1B ATYP][DST.ADDR]
  [DST.PORT][DATA]`, RFC 1928 §7) and maps to one `StreamKind::Udp` mux
  stream per **(association, destination)** pair
  (`EdgeSession::open_udp_flow`, `crate::udp_flow::EdgeUdpFlow` framing each
  datagram `[2B BE len][payload]` on that stream, both directions —
  `mirage_control_plane::{encode,decode}_udp_datagram_frame`). A flow opens
  lazily on the first datagram to a new destination and is evicted after
  `UDP_FLOW_IDLE_TIMEOUT` (60s) of silence in BOTH directions — matching the
  edge/exit's own `UDP_SESSION_TIMEOUT`. `FRAG != 0` is refused (dropped,
  counted, logged once) — no reassembly. A payload over
  `UDP_PAYLOAD_MTU_SAFE_LEN` (1200 bytes, matching
  `SAFE_UDP_HANDSHAKE_PAYLOAD_LEN`'s own MTU reasoning) is dropped the same
  way. `MAX_DESTINATIONS_PER_ASSOCIATION` (32) bounds one association's
  worst case. The WHOLE association dies the instant its control TCP
  connection closes or errors, per RFC 1928 §7's own lifetime rule — driven
  by one `tokio::select!` between reading that connection to EOF and the
  relay loop.
- `mirage-subscriber-client countries --artefact <path>` — connects, prints
  every country the edge can serve (one per line), exits. Never starts the
  SOCKS5 listener.

### TUN over subscriber-client's SOCKS5 (UDP)

`mirage2tun`'s existing SOCKS5 UDP client
(`mirage2tun/crates/mirage-tun-{windows,linux}/src/udp_socks5.rs`,
`udp_proxy_forward`/`udp_proxy_session_task`) needed **zero changes** for
this — it already performs a real SOCKS5 UDP ASSOCIATE handshake against
whatever `socks5_addr` it is pointed at (`socks5_udp_associate`), keyed by
`(src_addr, dst_addr)` like a NAT table, with its own independent 30s idle
timeout (`UDP_IDLE_TIMEOUT`, tighter than and unrelated to this crate's 60s
— harmless, it just means TUN's own local relay session recycles sooner).
`crate::tun_feed::TunFeed` already points TUN's one shared chain
(`TUN_FEED_CHAIN_ID`) at THIS crate's own SOCKS5 listener via
`socks5_bind` (`chain.socks5_bind = Some(socks5_bind.to_string())`) — so a
TUN-intercepted UDP packet (games, voice, system DNS) rides:

```
TUN (mirage-tun-windows/linux) --SOCKS5 UDP ASSOCIATE--> subscriber-client's
crate::socks5 listener --StreamKind::Udp mux stream--> edge --native-UDP
FS chain--> mesh exit --> real destination
```

**Wire-shape constraint this interop depends on**: `mirage2tun`'s own
`build_socks5_udp_request` always sends `ATYP` 0x01 (IPv4) or 0x04 (IPv6) —
never a domain — because it intercepts already-resolved raw IP packets; and
its `parse_socks5_udp_response` only understands ATYP 0x01/0x04 in the
REPLY. `serve_udp_association` accommodates this by echoing the exact
`ATYP+ADDR+PORT` bytes from the ORIGINAL request back in every reply for
that flow (never re-deriving or re-resolving an address locally) — so a
TUN-originated IPv4/IPv6 request always gets an IPv4/IPv6-shaped reply back,
exactly the shape `mirage2tun`'s parser expects, whether or not the
underlying `StreamOpen` target string travelled as a resolved address or
(for a directly SOCKS5-aware caller, never TUN) an unresolved domain.

No TUN-side config points at a UDP-shaped entrance directly — TUN only ever
talks to this crate's own local SOCKS5 listener, which is the ONE surface
that turns both TCP CONNECT and UDP ASSOCIATE into mux streams to the edge.

### Retired (kept for reference, `src/_deprecated/`, not compiled)

`chain_build.rs` (per-entrance `ChainConfig` builder) and `udp_dial.rs`
(per-entrance UDP dial wired into the old SOCKS5 `UdpAssociateSupport::
DatagramEntrance` path) — both superseded by the mux session's own
`StreamOpen` framing. See `src/_deprecated/_DEPRECATED.md`.

## mirage-l0-sentinel

Маленький daemon, живёт в `crates/l0-sentinel/`. Читает `sentinel.toml`:

```toml
monitor_url        = "http://127.0.0.1:19410"
monitor_api_key    = "<Client role key>"   # для WS auth, etap B/D
client_config_path = ".../client-config-managed.json"
client_binary      = ".../mirage-client-cli.exe"
client_args        = ["--config", "{client_config_path}"]
client_template    = ".../client-template.json"  # base ClientConfig без chains
poll_interval_secs = 30
respect_user_config = true
```

Поведение:
1. Спавнит `MonitorLink` (WS-first → HTTP poll fallback) с переданным `monitor_api_key`
2. Ждёт первый snapshot (timeout 30s)
3. Материализует ClientConfig = template + chains[] + `_managed_by: "mirage-l0-sentinel"`
4. Пишет `client_config_path` атомарно (tmp + rename)
5. Спавнит `client_binary` как child
6. На каждое обновление от монитора — переписывает файл (рестарт cli пока вручную, v2)

**Marker `_managed_by`**: если файл уже существует **без** этого маркера и `respect_user_config=true` — sentinel **не перезаписывает**, просто спавнит cli с существующим файлом. Это для пользовательских конфигов (file/QR) — sentinel их не трогает.

## AccessPacket v2 — identity-bearing sentinel registration (K2, 2026-09-25)

`crates/l0-sentinel/src/access_packet.rs` now accepts a SECOND delivery
shape at `access_path` (default `${state_dir}/access.json`, or `--access
<path>`), alongside the original `AccessPacket` (v1, still parsed and
applied exactly as before). `entrance_artefact::detect_kind` still tells an
`EntranceArtefact` from a `Packet` (both v1 and v2 carry `monitor`, no
`entrances`); WITHIN the `Packet` arm, `access_packet::detect_packet_version`
tells v1 from v2 by shape — v2 carries `declared_identity` and no
`client_token`, v1 the reverse.

**`AccessPacketV2`** — minted by mirage2panel's mode-3 private-mesh issuance
(`mirage2panel::mesh_access_packet::MeshAccessPacket`, mirrored field for
field): `v` (`2` — its own version namespace, independent of v1's
`CURRENT_VERSION`), `mesh_id`, `self_id`, `client_private_key_hex` /
`client_pubkey_hex` (X25519 identity keypair), `client_signing_private_key_hex`
/ `client_signing_pubkey_hex` (Ed25519 signing keypair — ALWAYS present in
v2, unlike the panel's own optional field on other issuance paths: a v2
device cannot register at all without one, since `possession_sig` is signed
with it), `declared_identity` (the operator-signed `ServiceRole::Sentinel`
`mirage_core::identity::DeclaredIdentity`, parsed as the real typed value —
unlike the panel, which keeps it opaque, this crate already depends on
`mirage-core` for other types), `monitor` (`AccessPacketV2Monitor`:
`wire_addr` + `wire_pubkey_hex`, same as v1's monitor, PLUS `self_id` — the
target monitor's own operator-declared identifier, needed for the
registration preimage below), `credential_params` (nullable, same as v1),
`issued_at_ms`, `label`.

**`AccessPacketV2::validate`** checks, by name (`AccessPacketError` gains the
v2-specific variants): version, non-empty `mesh_id`, `self_id` parses as a
UUID AND equals `declared_identity.self_id`, `declared_identity.roles`
contains `Sentinel`, `declared_identity.mesh_id` matches the packet's own
`mesh_id`, every hex field's shape (32 bytes each), `monitor.wire_addr` is
`host:port`, `monitor.self_id` is non-empty, and finally that BOTH key pairs
are actually pairs — the X25519 public key is truly derived from the private
key (`x25519_dalek`), and the Ed25519 verifying key is truly derived from the
signing key (`ed25519_dalek`). Deliberately does NOT verify
`declared_identity.operator_sig` — this crate has no pinned operator key at
packet-apply time; that signature is the MONITOR's job at registration time,
exactly as for the existing `/relay/clients/edge` route.

**`AccessPacketV2::apply`** persists, in order: (1) monitor wire coordinates
via the SAME `monitor_wire::save` v1 uses — so `lib.rs::run`'s existing
`wire_coords` resolution needs no v2-specific plumbing at all; (2)
`credential_params`, if present, to the SAME seed file v1 uses; (3)
`client-id.txt` / `sentinel-id.txt` forced to `self.self_id` — a v2 device's
`client_id` on registration success is ALWAYS `declared_identity.self_id`,
mirroring the monitor's Edge-registration rule; (4) the packet itself
(private keys included) to `${state_dir}/sentinel-device-identity.json`,
via `write_owner_only_atomic` (unix `chmod 0600` on the tmp file before
rename — same convention as `mlc-updater::token_store::save_token`; on
Windows this relies on the state dir's default per-user-profile ACL, same
precedent). **Never logged** — `AccessPacketV2` has a hand-written `Debug`
impl that redacts both private-key fields, so even an accidental `{:?}`
cannot leak them; the one INFO line `apply()` itself emits names only
`self_id`/`mesh_id`/`label`.

**Registration** (`lib.rs::run`): on every boot, `access_packet::
load_device_identity(&state_dir)` reads the persisted v2 record back (`Some`
regardless of whether THIS boot's `access_path` carried a fresh delivery —
the cache is what decides, mirroring how `wire_coords` itself falls back to
a persisted cache). When `Some`, the ordinary anonymous
`chain_request::register_with_failover` (`POST /relay/clients`) is never
called — instead `chain_request::register_sentinel_v2` calls `POST
/relay/clients/sentinel`, pinning the wire dial to `monitor.wire_pubkey_hex`
and presenting the device's OWN X25519 identity key as the V9 wire hello's
static key (`mirage_client_core::monitor_link::
wire_post_no_bearer_with_static_key` — new, mirrors `wire_post`'s HTTP/1.0
framing exactly, the two differences being a caller-supplied static key
instead of a fresh ephemeral one, and no `Authorization` header, since there
is no bearer yet). Body `{identity, nonce_hex, ts_ms, possession_sig,
shortname?}`:

- `nonce_hex` — 16 random bytes, hex (32 lowercase chars). Sourced from a
  fresh `uuid::Uuid::new_v4()`'s own bytes rather than adding a
  general-purpose RNG crate for one nonce — ample freshness salt alongside a
  bounded `ts_ms` window.
- `possession_sig` — Ed25519 (`mirage_core::identity::SignatureHex`) by the
  device's OWN signing key over `chain_request::
  sentinel_register_signing_bytes(self_id, mesh_id, monitor.self_id,
  nonce_hex, ts_ms)`:
  ```text
  b"MIRAGE-SENTINEL-CLIENT-REGISTER-V1\0"
  self_id.to_string() 0x00
  mesh_id             0x00
  monitor.self_id     0x00
  nonce_hex           0x00
  ts_ms.to_string()
  ```
  Field-for-field the SAME layout as `mirage2monitor::clients_api::
  edge_register_signing_bytes` (just this route's own label and this
  packet's own `monitor.self_id` in the `monitor_self_id` slot) — the
  monitor side (branch `sentinel-reg`) reproduces it byte-for-byte from its
  own `ApiState.own_self_id`-equivalent for this route. Byte-exact test
  vector: `chain_request::tests::
  sentinel_register_signing_bytes_matches_the_documented_encoding`.

On success (`{client_id, client_token}`, `client_id` always `self_id`) the
tuple `(active_monitor_url, client_id, client_token)` feeds straight into the
SAME code `register_with_failover` used to — the existing subscribe / config
/ chain-ask flow (`MonitorLink`, `chain_request::spawn_chain_asker`, …) is
completely unchanged and unaware a v2 device is behind it; those calls keep
using MonitorLink's own existing (ephemeral-per-dial) wire transport,
authenticated by the `client_token` bearer registration minted — the static
identity key applies to the REGISTRATION dial only.

On refusal (named `error`/`detail`, mirroring `EdgeClientRegisterError`'s own
wire labels — `bad_operator_sig`, `role_not_edge`→`role_not_sentinel` once the
monitor side lands, `revoked`, `mesh_mismatch`, …) or a transport failure,
`register_sentinel_v2` logs the reason BY NAME and backs off (30s floor / 5min
cap, the same schedule `chain_request::Backoff` already gives the K1
chain-ask cycle) before retrying — **never a tight spin, and no anonymous
fallback**: a v2 device has no `register_with_failover` escape hatch to fall
back to. The call blocks `run()` until it succeeds, same as the existing
`WaitingOnboard` gate blocks on missing wire coords.

**Monitor side**: `POST /relay/clients/sentinel` is being built concurrently
on `mirage2monitor` branch `sentinel-reg` against this exact contract — see
`mirage2monitor/CLAUDE.md`'s own "Edge client registration" section for the
closest existing precedent (`/relay/clients/edge`) this route mirrors.

## cli (`mirage-client-cli`) — 🔴 RETIRED 2026-06-14

> **Legacy. Не использовать.** cli1 перенесён в `crates/_deprecated/cli/` и
> полностью заменён cli2 (см. ниже + `_deprecated/cli/_DEPRECATED.md`). Убран из
> workspace members, `l0-orchestrator` dep (+ `ComponentKind::Cli`),
> `scripts/build-win-bins.sh`, и Windows-installer. Раздел ниже — историческая
> справка. E2E-доказательство замены: `nemo/docs/mirage/handoffs/handoff-2026-06-13-phase2-and-critiques.md` §8.

Читает `client-config.json` (включая `chains[]`, `monitor_url`, `monitor_api_key?`). Поднимает SOCKS5 listener'ы, TunnelPool/MuxPool на каждый чейн.

Для **direct chains** (`hops` пустой) при `use_mux=true` — строит `MuxPool` и использует его в `Socks5Server::run(pool, Some(mux_pool), ...)`. При сбое mux — fallback на TunnelPool runtime (не только при init).

Для **onion chains** mux работает с `onion_reply_mode=onion` (variant 2 — live с Apr 2026, see `mirage2node/CLAUDE.md`). Forward path raw post-RELAY, reply path layered AEAD.

### Data-plane carry: legacy (raw) vs modern (mux) boundary

ОДНА развилка решает путь несущей — `ChainConfig.reply` (worker `main.rs`:
`chain_wants_mux = use_mux && reply==Onion && !udp`):

- **`reply=Onion` → MuxPool** (modern, ДЕФОЛТ). Долгоживущие multiplexed
  carriers + circuit-breaker failover. Клиент шлёт `MUX\0`, exit апгрейдится
  в mux acceptor (etap-G, Variant 2). Monitor auto-provision выдаёт этот путь
  по умолчанию (`default_auto_axis_spec` → `reply_mode=Onion`, 2026-06-22).
- **`reply=Raw` → run_simple → TunnelPool** (LEGACY, явный opt-in). Single-use
  carriers, БЕЗ mux, БЕЗ failover — это ровно carry-семантика ретайрнутого
  cli1. Только UDP / debug. Никогда не дефолт.

cli1 retired в `_deprecated/`, но его carry-семантика выживает ИМЕННО как явный
`reply=Raw`. Регрессия 2026-06: monitor auto-chains дефолтили в `reply=Raw` →
cli2-клиент молча шёл по legacy `TunnelPool` (handshake green, carry flappy под
нагрузкой). Worker теперь логирует выбранный путь (`data-plane: MUX path` /
`LEGACY non-mux path` + reason) — boundary видно в логе, не угадывается.

`pool_size = 3` принудительно когда `use_mux=true` (3 TCP-несущих под mux достаточно для неограниченного числа streams).

## cli2 split-arch

Supervisor + N workers вместо одного процесса.

- **Worker** биндит pipe (`\\.\pipe\mirage-cli2-{chain_id}` Win, `${TEMP}/mirage-cli2/{chain_id}.sock` Unix), ждёт supervisor connect, отправляет `Hello { chain_id, pid, started_at_unix, version, state }`. Принимает `Start` / `Continue` / `Drain` / `Shutdown` / `Restart`.
- **Supervisor** на старте сканит `${TEMP}/mirage-cli2/*.pid`, проверяет процесс жив + connect к pipe → reattach к existing workers. Иначе — спавнит новый worker.
- Workers запускаются `DETACHED_PROCESS|CREATE_NEW_PROCESS_GROUP` (Win) / `setsid()` (Unix), `stdin = Stdio::null()` — переживают exit supervisor'а.
- `MIRAGE_WORKER_ORPHAN_TIMEOUT_SECS` (default 3600) — через сколько секунд без supervisor reconnect worker exits.

**Soft-restart**: supervisor умирает → workers держат pipe → новый supervisor стартует → discover_orphans → reattach. Трафик не рвётся.

**Windows pipe gotcha**: `PipeListener::bind` создаёт первый instance и хранит его в `pending`. `accept()` всегда await connect на pending, потом создаёт новый instance с `first_pipe_instance(false)`. Без этого dormant `_owner` instance ловил клиентов в зависание.

## OnionTunnel.raw_mode

`crates/client-core/src/onion_tunnel.rs` — `pub raw_mode: bool` field. Когда `true`, `recv()` делает plain `reader.read(buf)` без AEAD unwrap.

`establish_mux_tunnel` в `tunnel.rs` для onion chains устанавливает `raw_mode=true` сразу после `OnionTunnel::connect(..., ONION_MUX_MAGIC)`. Это нужно потому что при mux upgrade exit пишет raw mux frames — несовместимо с `[4B BE len][AEAD ciphertext]` framing onion-reply.

Forward direction уже raw post-RELAY frame для telescopic, поэтому send() не меняется.

## MonitorLink

`crates/client-core/src/monitor_link.rs` — WS subscribe к `/relay/clients/subscribe` с `Authorization: Bearer <monitor_api_key>` (Client role). На любой WS-error → fallback на 30s HTTP poll, retry WS каждые 5 минут.

`MonitorChain.chain` — полный `mirage_core::config::ChainConfig` (с pubkey/hops/protocol/...). Раньше был урезанный stub — теперь монитор отдаёт всё нужное чтобы клиент мог поднять chain без локального config (см. etap A в mirage2monitor).

## Reliability robustness

Каждый L0 процесс (cli, sentinel, supervisor, worker) умеет защищаться от утечек памяти и зомби-state. Четыре механизма:

### 1. Pool reaper (`client-core`)

`pool.rs::TunnelPool` и `mux_pool.rs::MuxPool` спавнят periodic reaper task в `new()`:

- Раз в минуту обходят слоты
- Слот в состоянии `Idle` без активности > `TUNNEL_POOL_IDLE_KILL_TIMEOUT` (300s) или > `MUX_POOL_IDLE_KILL_TIMEOUT` (600s) → close + переход в новое состояние `Dormant`
- `lease()` / `open_stream()` будит `Dormant` → `Reconnecting` on demand
- Reaper держит `Weak<Pool>` — когда сам pool drop'ается, task завершается на следующем tick

`switch_chain()` в обоих pools тоже будит `Dormant` слоты, чтобы они переподключились к новому чейну.

### 2. Memory guard (`mirage-runtime-guard`)

Новый shared crate `crates/runtime-guard/`. Каждый L0 binary вызывает `MemoryGuard::spawn(config, on_fatal)` при старте.

Background tokio task:
- Раз в `sample_interval` (default 10s) читает RSS процесса через `sysinfo`
- Считает % от системного RAM
- Threshold transitions: 70% → Warn, 85% → Critical, 95% → Fatal
- На Fatal вызывает `on_fatal` ровно один раз (caller сам решает: log + `exit(0)`/`exit(1)` для respawn, или graceful shutdown)

Дефолтная политика в L0 binaries: **`exit(0)` для clean respawn от sentinel/OS**. У supervisor дополнительно — kill всех children перед exit.

mirage2tun не может зависеть на `mirage-runtime-guard` через path (другой workspace) — там лежит локальная копия в `crates/mirage-tun-windows/src/runtime_guard.rs`.

### 3. Panic catcher (5 binaries)

Каждый main.rs устанавливает `std::panic::set_hook` сразу после `tracing_subscriber::init()`. На любой panic:
- Извлечь location + payload string
- `tracing::error!` с подробностями
- Sleep 100ms (чтобы tracing flush'нулся)
- `std::process::exit(1)` — clean exit, не abort

Это предотвращает **default Rust abort** (`SIGABRT` на Unix, `__fastfail` на Windows) и даёт sentinel/OS возможность респаунить процесс из чистого состояния.

### 4. Socket reaper в TUN (`mirage-tcpip-stack`)

См. документацию в `mirage2tun/` — reaper для smoltcp socket'ов внутри userspace TCP/IP stack. Аналогичный концепт.

### Cascade на pressure

Каскад при росте RSS:
1. **Warn (70%)** — лог, никаких действий
2. **Critical (85%)** — лог
3. **Fatal (95%)** — `on_fatal()` запускает graceful shutdown + `exit(0)` → sentinel/OS respawn

## Orchestrator (desktop + mobile portability)

`mirage-l0-orchestrator` — топ-уровневый supervisor над L0 компонентами. Два режима:

- **Process mode** (desktop): каждый компонент = отдельный child OS process. Crash isolation, soft-restart на OTA. Конфиг указывает `binary = "..."` для каждого компонента.
- **Task mode** (mobile/Android/iOS): каждый компонент = `tokio::task` внутри orchestrator-процесса. Sentinel/cli вызываются через их `pub async fn run(...)` библиотечные точки входа без процесса.

`Component` trait + `ComponentHandle` — абстракция над "как запустить и убить". `ProcessSpawner` (process.rs) и `TaskSpawner` (task.rs) — две impl. Supervisor итерирует `[[component]]` из `orchestrator.toml`, спавнит каждый, применяет `RestartPolicy` (always/on-failure/never), exponential backoff capped at 30s, Ctrl-C cascading kill.

На desktop sentinel **сам** спавнит cli — orchestrator управляет только sentinel. На mobile orchestrator спавнит обоих как tokio tasks (sentinel внутри своей логики тоже call'ит cli как process — но на mobile это будет другая ветка, см. TODO).

Embedded режим требует флагов `install_panic_hook=false`, `install_memory_guard=false` (sentinel) и `exit_on_memory_fatal=false` (оба) — orchestrator сам ставит общий panic hook + memory guard, embedded компонент не должен звать `process::exit()`.

## Builds

```bash
# Всё в mirage2app workspace
cd mirage2app
cargo build --release -p mirage-client-cli
cargo build --release -p mirage-l0-sentinel
cargo build --release -p mirage-l0-orchestrator
cargo build --release -p mirage-chain-supervisor -p mirage-chain-worker
```

GUI v2 (`crates/desktop/windows/v2/`) — экспериментальный, может отстать от uzor (имена крейтов меняются). При проблемах сборки workspace — проверить пути в `crates/desktop/windows/v2/Cargo.toml`.

### Testers — target CPU compatibility

- **tester1** = Win11 box (Intel Haswell, i5-4690K). **БЕЗ** AVX-512, **БЕЗ** SHA-NI. Бинарь, собранный с `target-cpu=native` на современной машине, **молча падает SIGILL** при старте (окно появляется и сразу пропадает, никаких логов). Симптом — лаунчер мигнул и ушёл.
- **tester2** = macOS (Intel Haswell). Та же проблема, тот же фикс.

**Перед отправкой бинарей тестерам ОБЯЗАТЕЛЬНО** сбрасывать `target-cpu` до безопасного.

**ВАЖНО**: `nemo/.cargo/config.toml` содержит:
```toml
[target.'cfg(any(windows, unix))']
rustflags = ["-C", "target-cpu=native"]
```

Это **перетирает** переменную окружения `RUSTFLAGS`. То есть `RUSTFLAGS="-C target-cpu=x86-64-v2" cargo build --release` **НЕ работает** — cargo возьмёт `native` из config.toml. Симптом: cargo рапортует `Finished release` за <1 сек (использует кэш с native), бинарь падает SIGILL у тестера.

**Рекомендуемый способ** — `nemo/scripts/build-win-bins.sh <profile>`. Каждый профиль пишет в **свой target-dir** (`nemo/target-build/win-<profile>/<crate>/release/...`), без перезаписи и `cargo clean` между прогонами:

```bash
bash nemo/scripts/build-win-bins.sh native     # для нашего dev box (target-cpu=native)
bash nemo/scripts/build-win-bins.sh tester1    # для tester1 Haswell (target-cpu=x86-64-v2)
bash nemo/scripts/build-win-bins.sh haswell    # явный haswell (полный набор Haswell-ISA фич)
bash nemo/scripts/build-win-bins.sh baseline   # x86-64 minimum, универсал
```

Профили живут параллельно: одна и та же команда `cargo run --release -p X` всегда возвращает наш-native бинарь из `target/release/`, а tester1-safe лежит отдельно в `target-build/win-tester1/<crate>/release/`. Отправляешь тестеру именно его сборку.

Под капотом: `--target-dir target-build/win-<profile>/<crate>` + `--config 'target."cfg(any(windows, unix))".rustflags=["-C","target-cpu=<cpu>"]'` (cargo 1.63+). Командная строка `--config` имеет приоритет над `nemo/.cargo/config.toml`, **никаких правок config.toml не нужно**.

**Альтернатива** (legacy) — временно править `nemo/.cargo/config.toml`:

```toml
[target.'cfg(any(windows, unix))']
rustflags = ["-C", "target-cpu=x86-64-v2"]   # tester-safe baseline
```

Затем `cargo clean -p <crate>` (cargo не пересобирает при смене rustflags автоматически без clean), затем build. После — вернуть `target-cpu=native`. Этот путь используем только если build-win-bins.sh почему-то не подходит.

Проверить какой baseline в текущем бинаре: `dumpbin /headers <exe> | findstr machine` (Win) или `otool -hv <exe>` (mac) не покажут CPU features напрямую — самый надёжный способ это запустить на тестере и смотреть, не падает ли.

## Phase 10 changes (May 2026) — ref: `docs/plans/phase-9-session-end-2026-05-15.md`

### 10.1.1 Wire V3 (mesh_id in ss_hint)

Client now sends `BLAKE3(ecdh || e_pub || "MIRAGE-AUTH-V3" || mesh_id)` as ss_hint.
Both sides enforce mesh_id — wrong-mesh handshake fails at server hint check, not just confirm.
See `mirage-dial-core/src/tunnel.rs`.

### 10.2.1 freshen_chain_pubkeys (per-attempt pubkey refresh)

`freshen_chain_pubkeys(chain, peer_table)` helper in `mirage-dial-core/src/tunnel.rs`.
Called at the top of every reconnect attempt (both `reconnect_slot` and `reconnect_mux_slot`).
Overrides `hop.public_key` from the current peer_table snapshot — discards stale static pub from initial `ClientConfig`. Fixes UDP client recovery after node ephemeral-keypair rotation.

### 9.7 is_onion gate (CRITICAL — fixes 10/10 camouflage smoke)

`is_onion = chain.hops.len() > 1` in TunnelPool + MuxPool + establish_mux_tunnel (5 sites).
Single-hop direct-exit chains with `hops=[Exit]` use raw path. Wrong `!hops.is_empty()` check
caused HTTP/0.9 + TLS alert + UDP timeout across all 10 camouflages — all closed by this fix.

## NEVER

- Не ходить в монитор напрямую из cli/cli2/tun — только через sentinel. Прямая связь — fallback.
- Не использовать stdin/stdout для cli2 IPC — это v0, переехали на named pipes для reattach.
- НЕ ставить `onion_reply_mode=raw` "чтобы включить mux" — это НАОБОРОТ legacy путь. Variant 2 (live Apr 2026, см. выше) mux'ит при `reply_mode=onion`; нода детектит mux по magic `MUX\0`, no per-hop flag. `reply=raw` уводит клиента на legacy single-use `TunnelPool` (`run_simple`, без mux/failover). См. "Data-plane carry" ниже.
