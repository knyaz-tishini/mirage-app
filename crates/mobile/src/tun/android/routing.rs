//! Android routing — intentional no-op.
//!
//! Route management is entirely handled by the Android OS via
//! `VpnService.Builder`:
//!
//! - `addRoute("0.0.0.0", 0)` / `addRoute("::", 0)` installs a catch-all
//!   route through the TUN fd on the Kotlin side.
//! - Bypass routes for relay endpoints are configured via
//!   `VpnService.Builder.addDisallowedApplication` or by leaving their UIDs
//!   out of the `addAllowedApplication` list — there is no Rust-side `ip route`
//!   command needed.
//!
//! Therefore all methods are no-ops that return `Ok(())`.

use std::net::IpAddr;

use mirage_core::error::MirageError;

use mirage_client_core::tun::traits::PlatformRouting;

/// No-op routing manager for Android — VpnService handles the route table.
pub struct AndroidRouting;

impl AndroidRouting {
    /// Create a new `AndroidRouting`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for AndroidRouting {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformRouting for AndroidRouting {
    /// No-op: default route is installed by `VpnService.Builder.addRoute`.
    fn set_default_route(&mut self, _tun_name: &str) -> Result<(), MirageError> {
        Ok(())
    }

    /// No-op: nothing was installed so nothing needs to be removed.
    fn restore_routes(&mut self) -> Result<(), MirageError> {
        Ok(())
    }

    /// No-op: relay bypass is configured on the Kotlin side via
    /// `addDisallowedApplication` or UID exclusions.
    fn add_bypass(&mut self, _addr: IpAddr) -> Result<(), MirageError> {
        Ok(())
    }
}
