# DEPRECATED — mirage-desktop-cli

**Retired 2026-06-14. Do not use, do not build, do not reference.**

Legacy pre-orchestrator launcher: it spawned `mirage-client-cli.exe` (cli1) +
`mirage2tun.exe` directly as child processes. Superseded by
**`mirage-l0-orchestrator`** (the supervision root over sentinel + the cli2
data-plane) — see `crates/l0-orchestrator/`. It referenced the now-retired cli1.

Delete outright once nobody needs to diff against it.
