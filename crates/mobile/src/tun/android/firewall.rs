//! Android firewall — intentional no-op.
//!
//! `VpnService` on Android handles all traffic routing and kill-switch
//! behaviour at the OS level:
//!
//! - All traffic from allowed apps is automatically routed through the TUN fd.
//! - Split-tunnel (per-app allow/block lists) is configured via
//!   `VpnService.Builder.addAllowedApplication` / `addDisallowedApplication`
//!   on the Kotlin side before the fd is handed to Rust.
//! - A kill switch can be enabled by calling
//!   `VpnService.Builder.setBlocking(true)` or using the system-level
//!   "Always-on VPN" setting — no Rust-side iptables required.
//!
//! Therefore this implementation accepts all calls and returns `Ok(())`.

use std::net::SocketAddr;

use mirage_core::error::MirageError;

use mirage_client_core::tun::traits::PlatformFirewall;

/// No-op firewall for Android — VpnService handles traffic policy.
pub struct AndroidFirewall;

impl AndroidFirewall {
    /// Create a new `AndroidFirewall`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for AndroidFirewall {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformFirewall for AndroidFirewall {
    /// No-op: Android routes all app traffic through the TUN fd automatically.
    fn activate(
        &mut self,
        _relay_addrs: &[SocketAddr],
        _tun_name: &str,
    ) -> Result<(), MirageError> {
        Ok(())
    }

    /// No-op: nothing was installed so nothing needs to be removed.
    fn deactivate(&mut self) -> Result<(), MirageError> {
        Ok(())
    }
}
