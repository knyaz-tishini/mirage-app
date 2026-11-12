pub mod log_capture;
pub mod tun;
pub mod discovery;
pub mod dns_forwarder;
pub mod endpoint_protocol;
pub mod http_proxy;
pub mod management;
pub mod runtime;
pub mod socks5;
pub mod topology_forwarder;
pub mod transparent;

// The chain machinery — the tunnel pool, the mux pool, the chain set and the
// token pools they spend from — moved into the OSS crate `mirage-chain-pool`
// (in-tree at `mirage-oss/crates/mirage-chain-pool/`), so that the relay
// edge can use the same pooling/muxing machinery the client uses instead of
// dialing a raw stream per client flow.
//
// Re-exported here under the SAME module paths, so every existing call-site
// (`use crate::pool::…`, `use crate::chain_set::…`, `use crate::mux_pool::…`,
// `use crate::relay::…`, `use crate::capability_pool::…`,
// `use crate::token_pool::…`, `use crate::wire_health::…`) keeps compiling
// unchanged.
pub use mirage_chain_pool::{
    capability_pool, chain_set, mux_pool, pool, relay, token_pool, wire_health,
};

pub use chain_set::{
    ChainSet, ChainHandle, ChainId, ChainState, ChainMetrics,
    PrewarmStrategy, HealthProbeConfig,
    router::{Router, RouterImpl, RoutingPolicy, TargetKey, FlowKey, ProtocolHint, BypassMatcher},
};

pub use token_pool::RingCredential;

/// Re-export dial primitives so existing call-sites
/// (`mirage_client_core::tunnel::*`, ...) continue to compile after the
/// dial-stack moved into `mirage-control-plane` (in-tree at
/// `mirage2web/crates/mirage-dial-core/` — folder rename pending IDE
/// lock release; crate name + lib name already migrated). Keeps cli /
/// sentinel / tun import paths stable.
pub use mirage_control_plane::cover_tunnel;
pub use mirage_control_plane::shaped_tunnel;
pub use mirage_control_plane::tunnel;
pub use mirage_control_plane::udp_framing;
