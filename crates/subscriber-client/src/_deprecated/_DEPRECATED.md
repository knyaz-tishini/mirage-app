# DEPRECATED — subscriber-client's per-entrance chain/UDP-dial modules

**Retired 2026-09-25.** Detached from the crate's module tree (`lib.rs` no
longer declares `mod chain_build;` / `mod udp_dial;`) — not compiled, not
referenced, kept for diff/reference only.

## `chain_build.rs`

Built one single-hop `mirage_core::config::ChainConfig` per artefact
entrance, consumed by `mirage_client_core::{ChainSet, TunnelPool}`. Replaced
by the edge-session mux model (`crate::session::EdgeSession`): the client no
longer builds a `ChainConfig` at all — it dials the artefact's entrance
addr/pubkey directly (`EdgeSession::connect`) and opens per-flow mux streams
with a `StreamOpen{Tcp, host:port}` header, never a fabricated chain.

## `udp_dial.rs`

Dialed a UDP-shaped entrance directly with the subscriber's persistent
identity (`UdpEntranceHandle`), wired into the old SOCKS5 server's
`UdpAssociateSupport::DatagramEntrance`. Replaced by a clean SOCKS5 UDP
ASSOCIATE refusal (`crate::socks5`, SOCKS5 reply `0x07` command-not-supported)
— the UDP relay riding the edge mux session is the next slice, per
`docs/mirage/plans/2026-09-24-edge-as-entry.md` PART TWO, "UDP datagram
framing".

Why parked here instead of deleted: kept for reference for one cycle;
**delete outright** once nobody needs to diff against it.
