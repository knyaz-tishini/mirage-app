//! iOS routing — intentional no-op.
//!
//! Route management on iOS is entirely handled by `NEPacketTunnelProvider`
//! through `NEPacketTunnelNetworkSettings`:
//!
//! - A catch-all default route through the TUN interface is expressed as
//!   `NEIPv4Route.defaultRoute()` / `NEIPv6Route.defaultRoute()` added to
//!   `includedRoutes` on the Swift side.
//! - Bypass routes for relay endpoints are expressed as `NEIPv4Route` /
//!   `NEIPv6Route` entries in `excludedRoutes` — no Rust-side `ip route`
//!   command is needed.
//! - The routing table is installed by the OS when `startTunnel` completes
//!   successfully, and torn down automatically when the tunnel stops.
//!
//! Therefore all methods are no-ops that return `Ok(())`.

use std::net::IpAddr;

use mirage_core::error::MirageError;

use mirage_client_core::tun::traits::PlatformRouting;

/// No-op routing manager for iOS — `NEPacketTunnelProvider` handles the route
/// table through `NEPacketTunnelNetworkSettings`.
pub struct IosRouting;

impl IosRouting {
    /// Create a new `IosRouting`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for IosRouting {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformRouting for IosRouting {
    /// No-op: default route is installed by `NEIPv4Route.defaultRoute()` on
    /// the Swift side.
    fn set_default_route(&mut self, _tun_name: &str) -> Result<(), MirageError> {
        Ok(())
    }

    /// No-op: nothing was installed so nothing needs to be removed.
    fn restore_routes(&mut self) -> Result<(), MirageError> {
        Ok(())
    }

    /// No-op: relay bypass routes are configured via `NEIPv4Route` /
    /// `NEIPv6Route` entries in `NEPacketTunnelNetworkSettings.excludedRoutes`
    /// on the Swift side.
    fn add_bypass(&mut self, _addr: IpAddr) -> Result<(), MirageError> {
        Ok(())
    }
}
