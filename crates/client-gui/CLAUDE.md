# mirage-client-gui — agent rules

The desktop GUI that ships in the client bundle (uzor `AppBuilder` runtime).
Own Cargo workspace (not a member of the mirage2app workspace): always gate
with `--manifest-path mirage2app/crates/client-gui/Cargo.toml`, never
`--workspace` / `--all-targets`.

## Customer build vs dev-tools

- The **customer build** (default features): LocalHttp spawns one
  mirage-subscriber-client child and drives it at http://127.0.0.1:19421
  (session on/off, SOCKS, HTTP, TUN wish). It has no
  fleet/operator/demo content and no developer control endpoint.
- Everything developer-only lives in `src/dev/` behind the Cargo feature
  `dev-tools` (**off by default**): the uzor agent API (:17471), the profiling
  control server (:17472), the fps HUD, the mock engine (`dev::mock`) and the
  `--connected` flag. In a `dev-tools` build the engine is the mock unless
  `--live` is passed.
- Dev loop: `cargo run --release --features dev-tools --manifest-path
  mirage2app/crates/client-gui/Cargo.toml -- --skin planet`.
- Kept CLI flags in the customer build: `--skin bloom|planet` (default planet) and
  `--backend <name>` (support fallback for a broken GPU driver, e.g.
  `--backend tiny_skia`).

## Source gate

`tests/customer_sources.rs::customer_sources_have_no_operator_strings` scans
every file under `src/` except `src/dev/` for hoster names, the fleet/demo
identifiers (`FLEET`, `LINKS`, `OFFERS`, `MEMBERS`), `agent_api`, the
operator/demo flags, `zen-geldmaschine`, staging ids and any IPv4 literal other
than `127.0.0.1`. Run it before every commit that touches `src/`:

```bash
timeout 2700 cargo test --release --manifest-path mirage2app/crates/client-gui/Cargo.toml customer_sources
```

Anything that must not ship to a customer goes under `src/dev/`, never behind a
runtime flag in customer code.

## The configuration next to the app

The settings of the person live in files next to the binaries, not in the code:

| file | what is in it | who writes it |
|---|---|---|
| `standalone/host.json` | the settings of the window: the carry, the kit, the ports, the lists of sites and apps, the last target | the window, at every change |
| `standalone/guests.json` | the cabinet: the servers the person may use | the window |
| `standalone/artefact.json` | the artefact of the link | the link |
| `standalone/configs/<name>/` | the config of a target: the relays of the chain, the ports | the window |
| `bypass-preset.json` | the bypass of the machine: the applications that leave the tunnel, the names, the networks, the protected names | the window (see the plan) |

`config/bypass-preset.json` in this crate is the **shipped default** of the last
file: the window puts it next to the device when there is none, and the
person's own copy wins over it.

The device (`mirage2tun`) looks for its preset in this order:
`MIRAGE_BYPASS_PRESET` → `bypass-preset.json` next to its own binary →
`C:\ProgramData\mirage2tun\bypass-preset.json`. The file next to the
application is therefore the one that counts.

## Exit catalog

`nodes::CATALOG` is empty by design: the customer build carries no baked-in
exit list. The Connect tab lists `egress_countries` from the cabinet catalog
(`GET /api/public/catalog`). The edge welcome is not that list. The planet
still sizes itself from `CATALOG.len()`.
