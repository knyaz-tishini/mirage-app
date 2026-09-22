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
//! Four things, per `docs/mirage/plans/thin-customer-connector-2026-09-17.md`:
//! 1. Parse and validate the delivery artefact — [`artefact`].
//! 2. Translate it into the `ChainConfig` shapes `mirage-client-core`
//!    already consumes, one single-hop chain per published face, each
//!    dialed with the subscriber's own PERSISTENT static key instead of a
//!    fresh ephemeral one — [`chain_build`], [`dial`].
//! 3. Stand up a local SOCKS5 listener over the existing router (sharding
//!    across faces, default `StickyPerFlow`) — [`assembly`].
//! 4. Optionally drive the existing TUN bridge — [`tun_feed`].
//!
//! Everything below the artefact translation is REUSED, unmodified,
//! machinery from `mirage-client-core` (`ChainSet`, `RouterImpl`,
//! `Socks5Server`, `TunnelPool`, `TunBridge`) and one already-public
//! function from `mirage-protocol` (`driver::run_client_tcp_v9`). Neither
//! `mirage2web` nor `mirage-client-core` needed to change for this crate to
//! exist — see [`dial::StaticKeyProtocol`]'s module doc for why the
//! persistent-identity requirement did not need a `mirage2web` edit either.
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

pub use artefact::{ArtefactEntrance, ArtefactError, ArtefactMode, EntranceArtefact};
pub use assembly::{SubscriberClient, SubscriberClientError, SubscriberClientOptions};
pub use dial::StaticKeyProtocol;
