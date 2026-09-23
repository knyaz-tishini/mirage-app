//! `mirage-subscriber-client` — placeholder package name, needs owner
//! sign-off (`thin-customer-connector-2026-09-17.md` §8 #2).
//!
//! The commercial customer's own light, non-mesh client. It talks ONLY to
//! his own entrance box faces, holds no mesh credential, sees no hop list,
//! no node list, no monitor — matching
//! `docs/mirage/plans/ruling-2026-09-17-edge-boxes-trust-and-tiers.md` §3:
//! an edge terminates a paying customer and never hands him a peer table or
//! a place in the topology.
//!
//! Five things, per `docs/mirage/plans/thin-customer-connector-2026-09-17.md`
//! plus real UDP-entrance support (`docs/mirage/research/2026-09-23-edge-
//! egress-cli2-recon.md` §4):
//! 1. Parse and validate the delivery artefact — [`artefact`].
//! 2. Translate it into the `ChainConfig` shapes `mirage-client-core`
//!    already consumes, one single-hop chain per published TCP-shaped face,
//!    each dialed with the subscriber's own PERSISTENT static key instead
//!    of a fresh ephemeral one — [`chain_build`], [`dial`].
//! 2b. Every UDP-shaped face (`TransportCamouflage::is_udp()`) is instead
//!    wired as a [`udp_dial::UdpEntranceHandle`], dialed with the SAME
//!    persistent identity through the datagram-native V9 driver directly —
//!    [`udp_dial`].
//! 3. Stand up a local SOCKS5 listener over the existing router (sharding
//!    across TCP faces, default `StickyPerFlow`) for CONNECT, plus UDP
//!    ASSOCIATE routed to the UDP entrance when one exists — [`assembly`].
//! 4. Optionally drive the existing TUN bridge — [`tun_feed`].
//!
//! Everything below the artefact translation is REUSED machinery from
//! `mirage-client-core` (`ChainSet`, `RouterImpl`, `Socks5Server`,
//! `TunnelPool`, `TunBridge`) and already-public functions from
//! `mirage-protocol`/`mirage-control-plane` (`driver::run_client_tcp_v9`,
//! `tunnel::establish_native_udp_tunnel_addressed`). `mirage2web` never
//! needed to change — see [`dial::StaticKeyProtocol`]'s and [`udp_dial`]'s
//! own module docs for why the persistent-identity requirement did not need
//! a `mirage2web` edit for either transport. `mirage-client-core` gained one
//! small, additive, backward-compatible seam (`socks5::
//! DatagramEntranceDialer` / `UdpAssociateSupport`) so its existing SOCKS5
//! UDP ASSOCIATE machinery (RFC 1928 §7 framing, per-destination session
//! management) could be reused rather than re-implemented — every existing
//! caller (every `cli2` chain) defaults to `UdpAssociateSupport::Legacy` and
//! observes zero behaviour change.
//!
//! **What must not be in this crate, and why it structurally cannot be** —
//! see [`assembly`]'s module doc for the exact dependency-boundary argument
//! per item (no fleet constant, no node list, no chain composition, no
//! credential-scheme policy, no debug shell, no monitor link).

pub mod artefact;
pub mod assembly;
pub mod chain_build;
pub mod dial;
pub mod tun_feed;
pub mod udp_dial;

pub use artefact::{ArtefactEntrance, ArtefactError, ArtefactMode, EntranceArtefact};
pub use assembly::{SubscriberClient, SubscriberClientError, SubscriberClientOptions};
pub use dial::StaticKeyProtocol;
pub use udp_dial::{UdpDialError, UdpEntranceHandle};
