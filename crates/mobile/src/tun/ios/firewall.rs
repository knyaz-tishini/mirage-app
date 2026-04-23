//! iOS firewall — intentional no-op.
//!
//! On iOS, `NEPacketTunnelProvider` manages all traffic policy through
//! `NEPacketTunnelNetworkSettings`:
//!
//! - Routes are configured via `NEIPv4Settings` / `NEIPv6Settings` on the
//!   Swift side before the tunnel becomes active.
//! - DNS is set via `NEDNSSettings`.
//! - Split-tunnel (per-app or per-route exclusions) is expressed through
//!   `excludedRoutes` / `includedRoutes` in the network settings.
//! - A kill switch is achieved by returning an error from
//!   `startTunnel(options:completionHandler:)` or by using the "Always-on
//!   VPN" MDM profile — no Rust-side iptables or `nftables` is required.
//!
//! Therefore this implementation accepts all calls and returns `Ok(())`.

use std::net::SocketAddr;

use mirage_core::error::MirageError;

use mirage_client_core::tun::traits::PlatformFirewall;

/// No-op firewall for iOS — `NEPacketTunnelProvider` handles traffic policy.
pub struct IosFirewall;

impl IosFirewall {
    /// Create a new `IosFirewall`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for IosFirewall {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformFirewall for IosFirewall {
    /// No-op: iOS routes all app traffic through `NEPacketFlow` automatically.
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
