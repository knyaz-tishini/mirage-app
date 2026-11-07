//! Android-specific TUN, firewall, and routing implementations.
//!
//! On Android the VPN tunnel fd is created by the Kotlin `VpnService.Builder`
//! and passed to Rust via JNI.  Firewall and routing are handled entirely by
//! the Android OS — these implementations are intentional no-ops.

pub mod firewall;
pub mod routing;
pub mod tun;

pub use firewall::AndroidFirewall;
pub use routing::AndroidRouting;
pub use tun::AndroidTun;
