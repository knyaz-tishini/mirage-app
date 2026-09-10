pub mod chain_set;
pub mod health_watcher;
pub mod policy_push;
pub mod token_pool;
pub mod capability_pool;
pub mod log_capture;
pub mod monitor_channel;
pub mod monitor_link;
pub mod tun_bridge;
pub mod discovery;
pub mod dns_forwarder;
pub mod endpoint_protocol;
pub mod http_proxy;
pub mod management;
pub mod mux_pool;
pub mod pool;
pub mod relay;
pub mod runtime;
pub mod socks5;
pub mod topology_forwarder;
pub mod transparent;
pub mod wire_health;

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
