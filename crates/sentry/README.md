# mirage-sentry (MVP, tester1)

Always-on companion to `mirage-slavyanka` on a trusted box. Two jobs:

1. **Supervise slavyanka.** Spawn it as a managed child; on crash, back off
   exponentially and respawn (1s → 60s cap).
2. **Heartbeat.** Every `heartbeat_interval_secs` POST a JSON body to
   `<monitor_url>/relay/clients/{client_id}/heartbeat` with
   `Authorization: Bearer <monitor_api_key>`.

NOT in scope (see `mirage2monitor/docs/admin-layer-concept.md` for the full
design): signed-spawn manifests, hardware-token verification,
per-target compile-time keys, anti-replay nonce store, log streaming, debug
shell triggering. Sentry's only job is "is slavyanka up?".

## Operator workflow

### 1. Obtain a Client-role bearer key

On any LM:

```bash
./target/release/mirage2monitor.exe --genkey --role client --label tester1-sentry
```

Copy the raw key (the `Authorization: Bearer <…>`) and paste the printed
`[[api_keys]]` block into the LM's `config.toml`, then reload registry:

```bash
curl -X POST -H "Authorization: Bearer <operator-key>" \
  https://lm-nl-1.zen-geldmaschine.net:19443/relay/registry/reload
```

### 2. Drop `sentry.toml` on tester1

Default path: `C:\ProgramData\mirage\sentry.toml`. Example:

```toml
monitor_url           = "https://lm-nl-1.zen-geldmaschine.net:19443"
monitor_api_key       = "<raw client-role key>"
heartbeat_interval_secs = 30
slavyanka_binary      = "C:/Program Files/Mirage/mirage-slavyanka.exe"
slavyanka_config      = "C:/ProgramData/mirage/slavyanka.toml"

# Optional — where to persist sentry-client-id.txt
# state_dir = "C:/ProgramData/mirage/state"

# Restart-backoff bounds (defaults shown).
initial_backoff_secs = 1
max_backoff_secs     = 60
```

### 3. Run

Foreground (smoke test):

```powershell
"C:\Program Files\Mirage\mirage-sentry.exe" --config C:\ProgramData\mirage\sentry.toml
```

As a Windows service: register with `nssm` or `sc create` pointing at the
binary with the same `--config` arg. The launcher (`mirage-launcher.exe`)
currently does NOT supervise sentry; that's a follow-up.

### 4. Verify

On any LM, look up the client:

```bash
curl -H "Authorization: Bearer <operator-key>" \
  https://lm-nl-1.zen-geldmaschine.net:19443/relay/clients | jq
```

Sentry's first POST to `/relay/clients` registers a fresh `client_id` and
caches it at `<state_dir>/sentry-client-id.txt`. On every restart that id is
reused; heartbeats land in the existing client_registry rows the monitor
already exposes for `mirage-l0-sentinel`-managed clients.

## Heartbeat wire format

Sentry matches the monitor's existing `ClientHeartbeat` schema (no schema
change needed). Slavyanka's status (`running | crashed | unknown`) is encoded
inside the first `ClientChainStatus` entry as a synthetic chain name:

```json
{
  "active_chains": [
    {
      "chain_name": "slavyanka:running",
      "transport":  "reverse-ssh",
      "latency_ms": null,
      "bytes_tx":   0,
      "bytes_rx":   0,
      "errors":     0,
      "connections": 1
    }
  ],
  "total_bytes_tx": 0,
  "total_bytes_rx": 0,
  "uptime_secs":    137
}
```

When we want a proper status field on the monitor side, extend
`mirage2monitor_common::types::ClientHeartbeat` with an optional
`slavyanka_status: Option<String>` and replace the synthetic chain hack here.

## Build

```bash
cd nemo/mirage2app
cargo check -p mirage-sentry
cargo build --release -p mirage-sentry \
  --target-dir target-build/win-tester1/mirage2app \
  --config 'target."cfg(any(windows, unix))".rustflags=["-C","target-cpu=x86-64-v2"]'
```

`build-win-bins.sh` needs a `sentry)` token entry alongside `debug-shell)` —
add it when we ship the first real Win release of sentry.

## Caveats / TODOs

- Reqwest with `rustls-tls` accepts Let's Encrypt out of the box (workspace
  feature set already includes `rustls-tls`). No cert pinning by design.
- Sentry currently does NOT stream slavyanka stdout/stderr upstream — it just
  inherits them. If you want them in `journalctl` / Windows event log, redirect
  via the service supervisor.
- The supervisor does not implement "stable enough → reset backoff" logic; the
  delay grows unbounded inside the configured cap until a clean shutdown. For
  the MVP that's fine; we can mirror the cli respawn-window pattern from
  `l0-sentinel/src/supervisor.rs` later if respawn loops get noisy.
