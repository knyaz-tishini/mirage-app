pub mod health_watcher;
pub mod log_capture;
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
pub mod transparent;

/// Re-export dial primitives so existing call-sites
/// (`mirage_client_core::tunnel::*`, `mirage_client_core::onion_tunnel::*`,
/// ...) continue to compile after the dial-stack moved into
/// [`mirage-dial-core`](https://crates.io/crates/mirage-dial-core) (in-tree
/// at `mirage2web/crates/mirage-dial-core/`). Keeps cli / sentinel / tun
/// import paths stable.
pub use mirage_dial_core::cover_tunnel;
pub use mirage_dial_core::onion_tunnel;
pub use mirage_dial_core::shaped_tunnel;
pub use mirage_dial_core::tunnel;
pub use mirage_dial_core::udp_framing;
