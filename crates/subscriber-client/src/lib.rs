//! `mirage-subscriber-client` — placeholder package name, needs owner
//! sign-off (`thin-customer-connector-2026-09-17.md` §8 #2).
//!
//! The commercial customer's own light, non-mesh client. It talks ONLY to
//! his own edge box's faces, holds no mesh credential, sees no hop list, no
//! node list, no monitor — matching
//! `docs/mirage/plans/2026-09-24-edge-as-entry.md` PART TWO: the client
//! knows only its edge, and every downstream choice (which chain, which
//! exit, which node) is the edge's own job.
//!
//! Five things:
//! 1. Parse and validate the delivery artefact — [`artefact`]. The artefact
//!    IS the config shape the operator hands the customer: the edge's
//!    faces (`host:port` + camouflage), the edge's own static key (one per
//!    face, normally identical across every face of the same edge), the
//!    customer's own persistent static key, and a default exit preference
//!    (full-auto or a named country) — see `mirage2app/CLAUDE.md`
//!    "subscriber-client config (edge session)".
//! 2. Dial one persistent [`session::EdgeSession`] over the artefact's
//!    TCP-shaped entrance faces (round-robin failover), exchange
//!    `EdgeHello`/`EdgeWelcome`, and — once accepted — upgrade to a mux
//!    session. [`session::EdgeSessionSupervisor`] holds this session up
//!    across drops with exponential-backoff reconnect. This replaces the
//!    crate's earlier one-single-hop-dial-per-flow path — see
//!    [`assembly`]'s own module doc.
//! 3. Stand up a local SOCKS5 listener (`crate::socks5`) — CONNECT opens
//!    one mux stream per flow; UDP ASSOCIATE (`crate::udp_associate`) opens
//!    one `StreamKind::Udp` mux stream per (association, destination) pair,
//!    per RFC 1928 §7 — see `mirage2app/CLAUDE.md` "subscriber-client config
//!    (edge session)".
//! 4. Optionally drive the existing TUN bridge — [`tun_feed`].
//! 5. Optionally publish `edge-status.json` — [`status`] — the connector's
//!    state (`connecting`/`connected`/`reconnecting`/`refused`/`fatal`) with a
//!    closed reason code, the requested and granted exit, and the edge's
//!    country list, rewritten atomically on every transition.
//!
//! **What must not be in this crate, and why it structurally cannot be**:
//! no dependency on `mirage_core::crypto::credential` (no ring membership,
//! no blind-RSA batch, no signed-capability pool reachable even as an
//! import), no `mirage_client_core::monitor_link::MonitorLink` import (no
//! monitor WS dial, no debug-tunnel listener), no `mirage-debug-shell`
//! dependency. `crate::dial::StaticKeyProtocol` (the persistent-identity V9
//! dial adapter) and `crate::tun_feed` are unchanged and reused as-is.
//!
//! `chain_build.rs` (per-entrance `ChainConfig` builder) and `udp_dial.rs`
//! (per-entrance UDP entrance dial, wired into the old SOCKS5 UDP
//! ASSOCIATE path) are retired to `src/_deprecated/` — superseded by the
//! mux session's own `StreamOpen{Tcp,..}` framing and, for UDP, by
//! `crate::udp_associate`'s real `StreamOpen{Udp,..}`-carried relay.

pub mod artefact;
pub mod assembly;
pub mod claim;
pub mod claim_image;
pub mod control;
pub mod dial;
pub mod http_connect;
pub mod peer;
pub mod scope;
pub mod session;
pub mod socks5;
pub mod status;
pub mod tun_feed;
pub mod tun_mode;
pub mod udp_associate;
pub mod udp_flow;
pub mod watchdog;

pub use artefact::{ArtefactEntrance, ArtefactError, ArtefactMode, EntranceArtefact};
pub use claim::redeem_artefact_json;
pub use assembly::{SubscriberClient, SubscriberClientError, SubscriberClientOptions};
pub use scope::EgressScope;
pub use tun_mode::TunMode;
pub use session::{
    exit_preference_from_mode, EdgeDialError, EdgeSession, EdgeSessionSupervisor, EdgeStreamError,
};
pub use status::{
    exit_label, publish_fatal, EdgeReason, EdgeState, EdgeStatusFile, ExitStatus, StatusWriter,
};
pub use udp_associate::{serve_udp_association, UdpAssociateError};
pub use udp_flow::EdgeUdpFlow;
pub use watchdog::{StreamHealth, WatchedStream, DEFAULT_STREAM_FAILURE_THRESHOLD};
