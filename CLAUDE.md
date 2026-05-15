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

## Universal `mirage-sentry` + stable cross-LM identity (May 2026)

One crate `crates/sentry/` ships as the **universal sentry binary** — same source deploys to every box (9 server-LMs + N client testers). Mode picked at build time via feature flags:

| Feature | `SENTRY_ROLE` const | Where |
|---|---|---|
| `client-mode` (default) | `sentry-client` | tester1 Win, tester2 Mac, future end-user clients |
| `server-lm-mode` | `sentry-server-lm` | each of the 9 LM boxes |
| `server-node-mode` | `sentry-server-node` | reserved for relay/exit-only nodes |

Build env:

```bash
SENTRY_SHORTNAME=tester1 cargo build --release -p mirage-sentry           # client (default)
SENTRY_SHORTNAME=lm-nl-1 cargo build --release -p mirage-sentry \
    --no-default-features --features server-lm-mode \
    --target x86_64-unknown-linux-musl
```

`SENTRY_SHORTNAME` is **required at build** — `env!` macro panics at compile time if missing. Two reasons:
- Becomes `SENTRY_SHORTNAME` const that sentry sends in register body so operator can identify the box at a glance.
- Selects `env/local/slavyanka-<shortname>-ed25519` for slavyanka's `include_str!` embed (each binary embeds a *different* SSH private key, popping one tester loses only its own).

Convenience scripts:
- `scripts/build-musl-sentry.sh <shortname> [feature]` — Docker musl cross-compile per shortname into `nemo/target-build/sentry-<shortname>/`.
- `scripts/mirage-sentry.service` — Linux systemd unit (root, restart=always).
- `scripts/com.zengeld.mirage-sentry.plist` — macOS LaunchAgent (`dev` user, KeepAlive).

### Stable `sentry_id` flow

1. First boot: `ensure_sentry_id(state_dir)` reads `${state_dir}/sentry-id.txt`; if missing, generates UUID v4 and writes atomically (tmp + rename). On subsequent boots the same id is reused.
2. Register on every reachable LM in parallel, sending `{sentry_id, role, shortname, os, version, supported_transports}`. Monitor (post May-2026 schema, see `mirage2monitor/CLAUDE.md`) accepts the caller-supplied `sentry_id` verbatim as `client_id` — so the same physical box ends up with one identical `client_id` across all 9 LMs instead of 9 random UUIDs.
3. **Stale-state guard**: if persisted `sentry-state.json` was written by a pre-`sentry_id` build (per-LM `client_id` doesn't match `sentry-id.txt`), wipe and re-register so the converged stable id propagates. No manual cache clear on upgrade.
4. State directory:
   - Linux/root: `/root/.local/share/mirage/`
   - macOS `dev`: `/Users/dev/.local/share/mirage/`
   - Windows: `%LOCALAPPDATA%/mirage/state/`
5. Operator-side listing: `GET /relay/clients` on any LM returns rows with `role` + `shortname`; same `client_id` appearing on multiple LMs = same physical sentry, just multi-homed.

### Slave-mode (slavyanka)

`crates/sentry/` spawns `mirage-slavyanka` as an in-process `tokio::task` (NOT a child binary) on `WsServerFrame::debug_tunnel_open { reverse_port }`. Slavyanka opens reverse SSH from the tester out to the dev-box and asks for a remote-port forward back to its own `127.0.0.1:31337` shell wrapper (`cmd.exe` on Windows / `/bin/sh -i` elsewhere). Operator on dev-box does `nc localhost <reverse_port>` and lands in a shell on the tester.

Each sentry embeds its own ed25519 SSH key (`env/local/slavyanka-<shortname>-ed25519`); matching `.pub` must live in dev-box's `~/.ssh/authorized_keys` with a per-shortname comment for auditability.

### Operator triggers slave-mode

```bash
. nemo/env/mirage2monitor/mirage2monitor.env
SID=<sentry_id from /relay/clients>            # 32-byte UUID, same across all LMs
RP=22500                                       # any unused port on dev-box
curl -sk -X POST -H "Authorization: Bearer $LM_NL_OPERATOR_KEY" \
  "https://lm-nl-1.zen-geldmaschine.net:19443/relay/clients/$SID/debug/enable" \
  -H "Content-Type: application/json" -d "{\"reverse_port\":$RP}"
# Then on dev-box:
nc localhost $RP                                # shell drops in
```

`POST .../debug/disable` ends the session and SIGTERMs slavyanka.

## Sentry / probe terminology (May 2026)

**Sentry** = the always-on monitoring agent on the client; in this workspace that's `mirage-l0-sentinel`. The matching role on a relay/exit **node** is the `mirage2monitor-probe` agent (in the `mirage2monitor` crate) — same concept, two binaries because client and node deployment shapes differ. **Probe** (the noun, distinct from the node-side sentry whose binary happens to be named `mirage2monitor-probe`) = the on-demand SSH-debug worker the sentry spawns when an operator hits the debug API; today's `mirage-debug-shell` binary, conceptually being renamed to `mirage-probe`. From this workspace's perspective only the client-side path matters: monitor pushes `WsServerFrame::DebugTunnelOpen` over the existing WS subscription, sentinel writes `${state_dir}/debug-tunnel.json` and execs the SSH-debug worker. Nodes use a parallel HTTP-driven flow handled entirely on the LM ↔ probe (sentry) leg, documented in `mirage2monitor/CLAUDE.md`.

## Debug-shell + dynamic monitor URLs (Apr 2026)

New crate `crates/debug-shell/` (`mirage-debug-shell`) — operator-triggered reverse-SSH tunnel. Sentinel reacts to two new WS frames from monitor (`monitor_link::WsServerFrame::DebugTunnelOpen { ssh_priv_key_pem, monitor_ssh_host, reverse_port, ttl_secs, ... }` and `DebugTunnelClose`); on Open it writes `${state_dir}/debug-tunnel.json` and spawns `mirage-debug-shell --config <path>`, on Close it SIGTERMs the child. `sentinel.toml` gains optional `debug_shell_binary` and `state_dir` fields. Same `WsServerFrame::Config` carries a new `peer_lm_urls: Vec<String>` field — sentinel persists it atomically to `${state_dir}/monitor_urls.json` and bootstraps from that file on next start (preferring it over static `monitor_urls`); on every change while running it calls `MonitorLink::reconfigure(...)` to swap the internal URL list. WS frames use the tagged enum `{kind: "config" | "debug_tunnel_open" | "debug_tunnel_close"}` — old monitors that emit untagged config still work via deserializer fallback. WS subscribe URL now carries `?client_id=<id>` query so monitor can route per-client pushes.

## Sentry vs probe — quick reference

In day-to-day usage this workspace deals only with the **client-side** sentry. The two-binary picture across both workspaces:

| Term | Binary | Where it runs | Lifecycle |
|------|--------|---------------|-----------|
| Sentry (client) | `mirage-l0-sentinel` | every desktop/mobile client | always-on; subscribes to LM via WS |
| Sentry (node) | `mirage2monitor-probe` | every relay/exit VPS | always-on; LM polls it over HTTP |
| Probe (debug worker) | `mirage-debug-shell` (deployed as `/usr/local/bin/mirage-probe`) | spawned by either sentry on demand | short-lived; one process per active reverse-SSH tunnel |

The debug-worker binary is **not** auto-started. Sentries spawn it on `POST /probe/spawn` (node side) or on a `WsServerFrame::DebugTunnelOpen` push (client side); they kill it on the matching disable/close.

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
| `crates/cli/` (`mirage-client-cli`) | SOCKS5/HTTP proxy server, TunnelPool/MuxPool, push в TUN через `/chains/update`. **Двойной крейт (lib + bin)** — `pub async fn run(CliRunOptions)` для embedded use |
| `crates/cli2/` (`supervisor` + `worker`) | Split-arch: один supervisor + N worker-процессов (по чейну) с reattach через named pipes — для soft-restart на OTA |

mirage2tun (отдельный submodule) — низкоуровневый TUN-перехватчик. Получает chains через `cli` POST. **Нельзя ходить в монитор напрямую из cli или tun** — только через sentinel. MonitorLink в самих cli/tun допустим только как fallback на случай мёртвого sentinel.

GUI потом натянется на этот headless backend как отдельная обёртка.

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

## cli (`mirage-client-cli`)

Читает `client-config.json` (включая `chains[]`, `monitor_url`, `monitor_api_key?`). Поднимает SOCKS5 listener'ы, TunnelPool/MuxPool на каждый чейн.

Для **direct chains** (`hops` пустой) при `use_mux=true` — строит `MuxPool` и использует его в `Socks5Server::run(pool, Some(mux_pool), ...)`. При сбое mux — fallback на TunnelPool runtime (не только при init).

Для **onion chains** mux работает с `onion_reply_mode=onion` (variant 2 — live с Apr 2026, see `mirage2node/CLAUDE.md`). Forward path raw post-RELAY, reply path layered AEAD.

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

## Reliability hardening

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
- Не запускать onion chain в mux mode без `onion_reply_mode=raw` на всех hops.
