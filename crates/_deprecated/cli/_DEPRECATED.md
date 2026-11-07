# DEPRECATED — mirage-client-cli (cli1)

**Retired 2026-06-14. Do not use, do not build, do not reference.**

This is the legacy single-process L0 data-plane (`mirage-client-cli`). It has
been fully replaced by **cli2** — the split-arch `mirage-chain-supervisor` +
per-chain `mirage-chain-worker` (`crates/cli2/`), which is OTA-safe (workers are
detached OS processes that survive a supervisor swap) and is the only data-plane
the launcher / orchestrator / installer ship.

Why parked here instead of deleted: kept for reference for one cycle; **delete
outright** once nobody needs to diff against it.

What replaced what:
- Production data-plane → `crates/cli2/supervisor` + `crates/cli2/worker`.
- Orchestrator data-plane component → `ComponentKind::Generic` spawning
  `mirage-chain-supervisor` (process mode). The old `ComponentKind::Cli`
  task-mode embedding was removed with this crate.
- Unified config → cli2-supervisor reads the sentinel's `ClientConfig` JSON via
  `Config::load_any` / `from_client_config`.

Removed from: the mirage2app workspace members, the `l0-orchestrator` path-dep,
`scripts/build-win-bins.sh`, and the Windows installer (`installer/windows/`).

E2E proof of the cli2 replacement: `docs/mirage/handoffs/handoff-2026-06-13-phase2-and-critiques.md` §8.
