# mirage2app — agent rules

## L0 stack

mirage2app содержит **headless backend клиента**. Три процесса работают в связке (на десктопе и в будущем — мобиле):

| Crate | Роль |
|-------|------|
| `crates/l0-sentinel/` (`mirage-l0-sentinel`) | Bootstrap: связь с монитором, материализация client-config, спавн cli |
| `crates/cli/` (`mirage-client-cli`) | SOCKS5/HTTP proxy server, TunnelPool/MuxPool, push в TUN через `/chains/update` |
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

Для **onion chains** mux работает только если все hops в chain имеют `onion_reply_mode=raw` на server side (см. `mirage2node/CLAUDE.md`).

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

## Builds

```bash
# Всё в mirage2app workspace
cd mirage2app
cargo build --release -p mirage-client-cli
cargo build --release -p mirage-l0-sentinel
cargo build --release -p mirage-chain-supervisor -p mirage-chain-worker
```

GUI v2 (`crates/desktop/windows/v2/`) — экспериментальный, может отстать от uzor (имена крейтов меняются). При проблемах сборки workspace — проверить пути в `crates/desktop/windows/v2/Cargo.toml`.

## NEVER

- Не ходить в монитор напрямую из cli/cli2/tun — только через sentinel. Прямая связь — fallback.
- Не использовать stdin/stdout для cli2 IPC — это v0, переехали на named pipes для reattach.
- Не запускать onion chain в mux mode без `onion_reply_mode=raw` на всех hops.
