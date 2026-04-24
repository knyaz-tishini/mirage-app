// Platform-specific integration points (future work).
//
// iOS: NetworkExtension (NEPacketTunnelProvider) — kernel-level VPN.
// Android: VpnService — requires BIND_VPN_SERVICE manifest permission.
// Desktop (Windows/macOS/Linux): system tray icon, auto-start on login.
//
// For now this module is empty — all functionality is handled by the
// uzor/winit layer.
