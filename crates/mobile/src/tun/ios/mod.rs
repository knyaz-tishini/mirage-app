//! iOS-specific TUN, firewall, and routing implementations.
//!
//! On iOS the VPN interface is managed entirely by `NEPacketTunnelProvider`.
//! The system provides a `NEPacketFlow` object (not a raw fd) that you call
//! `readPackets` / `writePackets` on from Swift/ObjC.
//!
//! The Rust side communicates with the Swift layer through a channel-based
//! handle ([`IosTunHandle`]): Swift pushes inbound packets in and reads
//! outbound packets out via C FFI functions exposed in `mirage-ffi`.
//!
//! Firewall and routing are no-ops — `NEPacketTunnelNetworkSettings` (DNS,
//! routes, split-tunnel rules) is configured entirely on the Swift side.

pub mod firewall;
pub mod routing;
pub mod tun;

pub use firewall::IosFirewall;
pub use routing::IosRouting;
pub use tun::{IosTun, IosTunHandle};
