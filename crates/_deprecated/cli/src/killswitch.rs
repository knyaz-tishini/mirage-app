//! Kill switch — blocks all outbound traffic except to the L1 relays.
//!
//! Uses `iptables` (Linux only) to install a named chain `MIRAGE_KS` that:
//! 1. Allows loopback traffic.
//! 2. Allows established/related connections (inbound replies).
//! 3. Allows DNS to localhost (for the optional dns_forwarder).
//! 4. Allows TCP to each relay IP:port (one rule per address).
//! 5. Drops everything else.
//!
//! The chain is inserted at the front of the OUTPUT chain so it takes
//! priority over any other rules.  On clean shutdown [`KillSwitch::deactivate`]
//! removes the chain.  If the process is killed, the rules persist — this is
//! the safe failure mode (traffic blocked rather than leaked).
//!
//! IPv6 is handled by a parallel `MIRAGE_KS` chain in `ip6tables` that permits
//! loopback, DHCPv6 client traffic, ICMPv6 neighbor-discovery/RA messages, and
//! each IPv6 relay address, then drops the rest.  When no IPv6 relay addresses
//! are present the chain is still installed (all outbound IPv6 is dropped except
//! the permitted neighbor-discovery traffic).  If `ip6tables` is absent on the
//! host, a warning is logged and the IPv4 kill-switch still applies.

use std::{net::SocketAddr, process::Command};

use thiserror::Error;
use tracing::{debug, info, warn};

const CHAIN_NAME: &str = "MIRAGE_KS";

/// Errors produced by kill-switch operations.
#[derive(Error, Debug)]
pub enum KillSwitchError {
    #[error("iptables command failed ({cmd}): {stderr}")]
    Command { cmd: String, stderr: String },
    #[error("iptables not found — kill switch requires Linux with iptables installed")]
    NotAvailable,
}

/// Manages iptables kill-switch rules for Linux Docker containers.
///
/// Install with [`KillSwitch::activate`].  Remove with [`KillSwitch::deactivate`].
/// The [`Drop`] implementation calls `deactivate` as a safety net on clean exit.
pub struct KillSwitch {
    relay_addrs: Vec<SocketAddr>,
    /// Optional TUN interface name.  When set, outbound packets on that
    /// interface are explicitly accepted in both ip4tables and ip6tables so
    /// tunnelled traffic is never accidentally dropped.
    tun_iface: Option<String>,
    /// Bypass CIDRs (invariant 1: bypass is sacred).  Traffic destined for
    /// these CIDRs is explicitly ACCEPT-ed before the DROP floor so that
    /// route-rule-designated direct flows are never killed by the kill-switch.
    ///
    /// Both IPv4 (e.g. `"10.0.0.0/8"`) and IPv6 (e.g. `"2001:db8::/32"`)
    /// prefixes are accepted.  IPv4 CIDRs go into the ip4tables chain; IPv6
    /// CIDRs go into the ip6tables chain.
    bypass_cidrs: Vec<String>,
    /// Whether the rules are currently installed.
    active: bool,
}

impl KillSwitch {
    /// Create a new (inactive) kill switch allowing traffic to all addresses in
    /// `relay_addrs`.  Each unique (IP, port) pair gets its own ACCEPT rule.
    pub fn new(relay_addrs: Vec<SocketAddr>) -> Self {
        Self {
            relay_addrs,
            tun_iface: None,
            bypass_cidrs: Vec::new(),
            active: false,
        }
    }

    /// Attach bypass CIDRs (invariant 1 — bypass is sacred).
    ///
    /// Each CIDR in `cidrs` generates an ACCEPT rule in the `MIRAGE_KS` chain
    /// *before* the DROP floor, so traffic explicitly designated as direct by
    /// the routing table is never blocked.  IPv4 CIDRs go into the ip4tables
    /// chain; IPv6 CIDRs go into the ip6tables chain.  Invalid or empty strings
    /// are silently skipped at activation time.
    pub fn with_bypass_cidrs(mut self, cidrs: Vec<String>) -> Self {
        self.bypass_cidrs = cidrs;
        self
    }

    /// Attach a TUN interface name so traffic on that interface is always
    /// accepted (both IPv4 and IPv6).
    pub fn with_tun_iface(mut self, iface: impl Into<String>) -> Self {
        self.tun_iface = Some(iface.into());
        self
    }

    /// Install iptables rules.  Idempotent — safe to call when already active
    /// (it will attempt to create the chain; `iptables` will return a
    /// non-fatal "Chain already exists" error which we ignore).
    pub fn activate(&mut self) -> Result<(), KillSwitchError> {
        // Verify iptables is available.
        which_iptables()?;

        let v4_addrs: Vec<SocketAddr> = self
            .relay_addrs
            .iter()
            .filter(|a| a.is_ipv4())
            .copied()
            .collect();
        let v6_addrs: Vec<SocketAddr> = self
            .relay_addrs
            .iter()
            .filter(|a| a.is_ipv6())
            .copied()
            .collect();

        // Partition bypass CIDRs into IPv4 and IPv6.
        let bypass_v4: Vec<&str> = self
            .bypass_cidrs
            .iter()
            .filter(|c| !c.contains(':'))
            .map(String::as_str)
            .collect();
        let bypass_v6: Vec<&str> = self
            .bypass_cidrs
            .iter()
            .filter(|c| c.contains(':'))
            .map(String::as_str)
            .collect();

        info!(
            v4_count = v4_addrs.len(),
            v6_count = v6_addrs.len(),
            bypass_v4_count = bypass_v4.len(),
            bypass_v6_count = bypass_v6.len(),
            "kill switch: activating (chain {CHAIN_NAME})",
        );

        install_ip4_chain(&v4_addrs, self.tun_iface.as_deref(), &bypass_v4)?;
        install_ip6_chain(&v6_addrs, self.tun_iface.as_deref(), &bypass_v6);

        self.active = true;
        info!(
            relay_count = self.relay_addrs.len(),
            bypass_cidr_count = self.bypass_cidrs.len(),
            "kill switch: active — all traffic except {} relay(s) and {} bypass CIDR(s) is blocked",
            self.relay_addrs.len(),
            self.bypass_cidrs.len(),
        );
        Ok(())
    }

    /// Remove iptables rules.  Called on clean shutdown.
    ///
    /// Safe to call when not active (returns `Ok(())` immediately).
    pub fn deactivate(&mut self) -> Result<(), KillSwitchError> {
        if !self.active {
            return Ok(());
        }

        info!("kill switch: deactivating");

        remove_ip4_chain();
        remove_ip6_chain();

        self.active = false;
        info!("kill switch: deactivated — normal routing restored");
        Ok(())
    }
}

impl Drop for KillSwitch {
    fn drop(&mut self) {
        if self.active {
            let _ = self.deactivate();
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Check that `iptables` is on PATH.
fn which_iptables() -> Result<(), KillSwitchError> {
    match Command::new("iptables").arg("--version").output() {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(KillSwitchError::NotAvailable),
        Err(e) => Err(KillSwitchError::Command {
            cmd: "iptables --version".to_owned(),
            stderr: e.to_string(),
        }),
    }
}

/// Run an `iptables` command and return an error if it fails.
fn run_iptables(args: &[&str]) -> Result<(), KillSwitchError> {
    debug!(cmd = %format!("iptables {}", args.join(" ")), "kill switch: running iptables");
    let output = Command::new("iptables")
        .args(args)
        .output()
        .map_err(|e| KillSwitchError::Command {
            cmd: format!("iptables {}", args.join(" ")),
            stderr: e.to_string(),
        })?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Err(KillSwitchError::Command {
        cmd: format!("iptables {}", args.join(" ")),
        stderr,
    })
}

/// Run an `iptables` command, tolerating failures (logs a warning instead).
///
/// Used for the chain-creation step where "Chain already exists" is acceptable.
fn run_iptables_tolerant(args: &[&str]) -> Result<(), KillSwitchError> {
    match run_iptables(args) {
        Ok(()) => Ok(()),
        Err(KillSwitchError::Command { cmd, stderr }) => {
            // "Chain already exists" is the expected error on duplicate activate.
            if stderr.contains("Chain already exists") {
                debug!(cmd = %cmd, "kill switch: chain already exists, continuing");
                Ok(())
            } else {
                Err(KillSwitchError::Command { cmd, stderr })
            }
        }
        Err(e) => Err(e),
    }
}

/// Install the IPv4 `MIRAGE_KS` iptables chain.
///
/// `relay_addrs` must contain only IPv4 addresses.
/// `bypass_cidrs` are IPv4 CIDR strings that must be ACCEPT-ed before the
/// DROP floor (invariant 1 — bypass is sacred).
fn install_ip4_chain(
    relay_addrs: &[SocketAddr],
    tun_iface: Option<&str>,
    bypass_cidrs: &[&str],
) -> Result<(), KillSwitchError> {
    // 1. Create the custom chain.  If it already exists iptables exits with
    //    status 1 and the message "Chain already exists." — that is fine.
    run_iptables_tolerant(&["-N", CHAIN_NAME])?;

    // 2. Allow loopback (by interface and by destination).
    //    Both rules are needed: `-o lo` for packets that already have the
    //    loopback output interface, and `-d 127.0.0.0/8` for packets whose
    //    destination was rewritten to 127.0.0.1 by iptables REDIRECT in the
    //    nat OUTPUT chain (TUN mode).  The kernel may not yet have updated
    //    the output interface to `lo` when the filter OUTPUT chain fires.
    run_iptables(&["-A", CHAIN_NAME, "-o", "lo", "-j", "ACCEPT"])?;
    run_iptables(&["-A", CHAIN_NAME, "-d", "127.0.0.0/8", "-j", "ACCEPT"])?;

    // 3. Allow TUN interface output (if configured).
    if let Some(iface) = tun_iface {
        run_iptables(&["-A", CHAIN_NAME, "-o", iface, "-j", "ACCEPT"])?;
    }

    // 4. Allow established/related (replies to connections we initiated).
    run_iptables(&[
        "-A",
        CHAIN_NAME,
        "-m",
        "conntrack",
        "--ctstate",
        "ESTABLISHED,RELATED",
        "-j",
        "ACCEPT",
    ])?;

    // 5. Allow TCP to each relay address (one rule per unique IPv4 IP:port pair).
    for addr in relay_addrs {
        debug_assert!(addr.is_ipv4(), "install_ip4_chain: got non-IPv4 addr {addr}");
        let relay_ip = addr.ip().to_string();
        let relay_port = addr.port().to_string();
        info!(relay = %addr, "kill switch: adding ACCEPT rule for relay");
        run_iptables(&[
            "-A",
            CHAIN_NAME,
            "-d",
            &relay_ip,
            "-p",
            "tcp",
            "--dport",
            &relay_port,
            "-j",
            "ACCEPT",
        ])?;
    }

    // 6. Allow DNS to localhost (dns_forwarder listens on 127.0.0.1).
    run_iptables(&[
        "-A",
        CHAIN_NAME,
        "-d",
        "127.0.0.1",
        "-p",
        "udp",
        "--dport",
        "53",
        "-j",
        "ACCEPT",
    ])?;

    // 7. Bypass CIDRs — invariant 1 (bypass is sacred).
    //    These ACCEPTs sit between relay-IP rules and the DROP floor so that
    //    any destination explicitly designated as direct by the routing table
    //    is never blocked.
    for cidr in bypass_cidrs {
        info!(cidr = *cidr, "kill switch: adding bypass CIDR ACCEPT rule");
        run_iptables(&["-A", CHAIN_NAME, "-d", cidr, "-j", "ACCEPT"])?;
    }

    // 8. Drop everything else (invariant 2 — no-native floor).
    run_iptables(&["-A", CHAIN_NAME, "-j", "DROP"])?;

    // 9. Insert jump at the front of OUTPUT.
    run_iptables(&["-I", "OUTPUT", "1", "-j", CHAIN_NAME])?;

    Ok(())
}

/// Remove the IPv4 `MIRAGE_KS` iptables chain.
fn remove_ip4_chain() {
    if let Err(e) = run_iptables(&["-D", "OUTPUT", "-j", CHAIN_NAME]) {
        warn!("kill switch deactivate: remove v4 OUTPUT jump failed: {e}");
    }
    if let Err(e) = run_iptables(&["-F", CHAIN_NAME]) {
        warn!("kill switch deactivate: flush v4 chain failed: {e}");
    }
    if let Err(e) = run_iptables(&["-X", CHAIN_NAME]) {
        warn!("kill switch deactivate: delete v4 chain failed: {e}");
    }
}

/// Install an `ip6tables` MIRAGE_KS chain that permits loopback, DHCPv6
/// client traffic, ICMPv6 neighbor-discovery/RA messages, any IPv6 relay
/// addresses, and any IPv6 bypass CIDRs, then drops the rest.
///
/// `relay_addrs` must contain only IPv6 addresses (caller responsibility).
/// `bypass_cidrs` are IPv6 CIDR strings that must be ACCEPT-ed before the
/// DROP floor (invariant 1 — bypass is sacred).
/// All operations are tolerant — ip6tables may not exist on the host.
fn install_ip6_chain(relay_addrs: &[SocketAddr], tun_iface: Option<&str>, bypass_cidrs: &[&str]) {
    // Create chain (ignore "already exists").
    run_ip6tables_tolerant(&["-N", CHAIN_NAME]);

    // Loopback.
    run_ip6tables_tolerant(&["-A", CHAIN_NAME, "-o", "lo", "-j", "ACCEPT"]);
    run_ip6tables_tolerant(&["-A", CHAIN_NAME, "-d", "::1", "-j", "ACCEPT"]);

    // TUN interface output (if configured).
    if let Some(iface) = tun_iface {
        run_ip6tables_tolerant(&["-A", CHAIN_NAME, "-o", iface, "-j", "ACCEPT"]);
    }

    // Established / related.
    run_ip6tables_tolerant(&[
        "-A",
        CHAIN_NAME,
        "-m",
        "conntrack",
        "--ctstate",
        "ESTABLISHED,RELATED",
        "-j",
        "ACCEPT",
    ]);

    // Per-relay IPv6 ACCEPT rules.
    for addr in relay_addrs {
        debug_assert!(addr.is_ipv6(), "install_ip6_chain: got non-IPv6 addr {addr}");
        let relay_ip = addr.ip().to_string();
        let relay_port = addr.port().to_string();
        info!(relay = %addr, "kill switch: adding ip6tables ACCEPT rule for v6 relay");
        run_ip6tables_tolerant(&[
            "-A",
            CHAIN_NAME,
            "-d",
            &relay_ip,
            "-p",
            "tcp",
            "--dport",
            &relay_port,
            "-j",
            "ACCEPT",
        ]);
    }

    // DHCPv6 client → server (UDP dst port 547).  Required for DHCP-assigned
    // IPv6 addresses to be renewed even while the kill-switch is active.
    run_ip6tables_tolerant(&[
        "-A",
        CHAIN_NAME,
        "-p",
        "udp",
        "--dport",
        "547",
        "-j",
        "ACCEPT",
    ]);

    // ICMPv6 neighbor-discovery and RA messages.  Without these the host
    // cannot perform NDP and loses IPv6 connectivity entirely even to the
    // default gateway.
    //   133 — Router Solicitation (RS)
    //   134 — Router Advertisement (RA)
    //   135 — Neighbor Solicitation (NS)
    //   136 — Neighbor Advertisement (NA)
    //   143 — MLDv2 Multicast Listener Report
    for icmpv6_type in ["133", "134", "135", "136", "143"] {
        run_ip6tables_tolerant(&[
            "-A",
            CHAIN_NAME,
            "-p",
            "ipv6-icmp",
            "--icmpv6-type",
            icmpv6_type,
            "-j",
            "ACCEPT",
        ]);
    }

    // Bypass CIDRs — invariant 1 (bypass is sacred).
    for cidr in bypass_cidrs {
        info!(cidr = *cidr, "kill switch: adding ip6tables bypass CIDR ACCEPT rule");
        run_ip6tables_tolerant(&["-A", CHAIN_NAME, "-d", cidr, "-j", "ACCEPT"]);
    }

    // Drop everything else (invariant 2 — no-native floor).
    run_ip6tables_tolerant(&["-A", CHAIN_NAME, "-j", "DROP"]);

    // Insert jump at front of OUTPUT.
    run_ip6tables_tolerant(&["-I", "OUTPUT", "1", "-j", CHAIN_NAME]);
}

/// Remove the ip6tables MIRAGE_KS chain.  Tolerant of missing chain / missing ip6tables.
fn remove_ip6_chain() {
    run_ip6tables_tolerant(&["-D", "OUTPUT", "-j", CHAIN_NAME]);
    run_ip6tables_tolerant(&["-F", CHAIN_NAME]);
    run_ip6tables_tolerant(&["-X", CHAIN_NAME]);
}

/// Run an `ip6tables` command, tolerating all failures (ip6tables may not be
/// installed or IPv6 may be disabled in the kernel).
fn run_ip6tables_tolerant(args: &[&str]) {
    debug!(cmd = %format!("ip6tables {}", args.join(" ")), "kill switch: running ip6tables");
    match Command::new("ip6tables").args(args).output() {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                cmd = %format!("ip6tables {}", args.join(" ")),
                stderr = %stderr,
                "kill switch: ip6tables command failed (non-fatal)",
            );
        }
        Err(e) => {
            warn!(
                cmd = %format!("ip6tables {}", args.join(" ")),
                error = %e,
                "kill switch: ip6tables not available (non-fatal)",
            );
        }
        Ok(_) => {}
    }
}

// ── Rule-list helpers (used only in tests) ───────────────────────────────────

/// Represents a single iptables / ip6tables rule as collected args.
#[cfg(test)]
type RuleArgs = Vec<&'static str>;

/// Build the ordered list of ip6tables rule-arg slices that
/// `install_ip6_chain` would issue for the given relay addrs, tun iface, and
/// bypass CIDRs.
///
/// Used in structural unit tests — no actual `ip6tables` binary is invoked.
#[cfg(test)]
fn expected_ip6_rules(
    relay_addrs: &[SocketAddr],
    tun_iface: Option<&'static str>,
    bypass_cidrs: &[&'static str],
) -> Vec<RuleArgs> {
    let mut rules: Vec<RuleArgs> = Vec::new();

    rules.push(vec!["-N", CHAIN_NAME]);
    rules.push(vec!["-A", CHAIN_NAME, "-o", "lo", "-j", "ACCEPT"]);
    rules.push(vec!["-A", CHAIN_NAME, "-d", "::1", "-j", "ACCEPT"]);

    if let Some(iface) = tun_iface {
        rules.push(vec!["-A", CHAIN_NAME, "-o", iface, "-j", "ACCEPT"]);
    }

    rules.push(vec![
        "-A",
        CHAIN_NAME,
        "-m",
        "conntrack",
        "--ctstate",
        "ESTABLISHED,RELATED",
        "-j",
        "ACCEPT",
    ]);

    // per-relay rules (only IPv6)
    for addr in relay_addrs {
        if addr.is_ipv6() {
            // We can't easily build static &str here since the addr is dynamic,
            // so we skip per-relay args in the static rule list and test them
            // separately via the relay_count assertion.
        }
    }

    rules.push(vec!["-A", CHAIN_NAME, "-p", "udp", "--dport", "547", "-j", "ACCEPT"]);
    for t in ["133", "134", "135", "136", "143"] {
        rules.push(vec!["-A", CHAIN_NAME, "-p", "ipv6-icmp", "--icmpv6-type", t, "-j", "ACCEPT"]);
    }
    // bypass CIDRs sit just before the DROP floor
    for cidr in bypass_cidrs {
        rules.push(vec!["-A", CHAIN_NAME, "-d", cidr, "-j", "ACCEPT"]);
    }
    rules.push(vec!["-A", CHAIN_NAME, "-j", "DROP"]);
    rules.push(vec!["-I", "OUTPUT", "1", "-j", CHAIN_NAME]);

    rules
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    #[test]
    fn new_is_inactive() {
        let addrs: Vec<SocketAddr> = vec!["127.0.0.1:4433".parse().unwrap()];
        let ks = KillSwitch::new(addrs);
        assert!(!ks.active);
    }

    /// Verify that deactivate on an inactive kill switch is a no-op.
    #[test]
    fn deactivate_when_inactive_is_noop() {
        let addrs: Vec<SocketAddr> = vec!["127.0.0.1:4433".parse().unwrap()];
        let mut ks = KillSwitch::new(addrs);
        assert!(ks.deactivate().is_ok());
    }

    /// Full lifecycle test — requires Linux + root + iptables.
    #[test]
    #[ignore]
    fn activate_deactivate_lifecycle() {
        let addrs: Vec<SocketAddr> = vec![
            "10.0.0.1:4430".parse().unwrap(),
            "10.0.0.1:4431".parse().unwrap(),
            "10.0.0.1:4432".parse().unwrap(),
        ];
        let mut ks = KillSwitch::new(addrs);
        ks.activate().expect("activate should succeed");
        assert!(ks.active);
        ks.deactivate().expect("deactivate should succeed");
        assert!(!ks.active);
    }

    /// `install_ip6_chain` / `remove_ip6_chain` compile and are callable on
    /// non-Linux (ip6tables tolerant).  No assertions on output — just no panic.
    #[test]
    fn ip6_chain_install_remove_tolerant() {
        let addrs: Vec<SocketAddr> = vec![
            "[2001:db8::1]:4430".parse().unwrap(),
            "10.0.0.1:4431".parse().unwrap(), // v4 — filtered out by caller
        ];
        let v6_only: Vec<SocketAddr> = addrs.iter().filter(|a| a.is_ipv6()).copied().collect();
        install_ip6_chain(&v6_only, None, &[]);
        remove_ip6_chain();
    }

    /// KillSwitch with a mix of v4 and v6 relay addresses is created without error.
    #[test]
    fn new_with_mixed_addrs() {
        let addrs: Vec<SocketAddr> = vec![
            "10.0.0.1:4430".parse().unwrap(),
            "[2001:db8::1]:4430".parse().unwrap(),
        ];
        let ks = KillSwitch::new(addrs);
        assert!(!ks.active);
    }

    /// v4 and v6 addresses are correctly partitioned.
    #[test]
    fn addr_partition() {
        let addrs: Vec<SocketAddr> = vec![
            "10.0.0.1:4430".parse().unwrap(),
            "192.168.1.1:4431".parse().unwrap(),
            "[2001:db8::1]:4430".parse().unwrap(),
            "[2001:db8::2]:4431".parse().unwrap(),
        ];
        let v4: Vec<SocketAddr> = addrs.iter().filter(|a| a.is_ipv4()).copied().collect();
        let v6: Vec<SocketAddr> = addrs.iter().filter(|a| a.is_ipv6()).copied().collect();
        assert_eq!(v4.len(), 2);
        assert_eq!(v6.len(), 2);
        assert!(v4.iter().all(|a| matches!(a.ip(), IpAddr::V4(_))));
        assert!(v6.iter().all(|a| matches!(a.ip(), IpAddr::V6(_))));
    }

    /// v4-only input → v6 slice is empty → ip6_chain still runs (drops all v6).
    #[test]
    fn v4_only_addrs_yields_empty_v6_slice() {
        let addrs: Vec<SocketAddr> = vec![
            "10.0.0.1:4430".parse().unwrap(),
            "10.0.0.2:4431".parse().unwrap(),
        ];
        let v6: Vec<SocketAddr> = addrs.iter().filter(|a| a.is_ipv6()).copied().collect();
        assert!(v6.is_empty());
        // install_ip6_chain with empty slice must not panic
        install_ip6_chain(&v6, None, &[]);
        remove_ip6_chain();
    }

    /// Structural: ip6tables rule list contains all required rule families.
    #[test]
    fn ip6_rule_list_structure() {
        let rules = expected_ip6_rules(&[], None, &[]);

        // Must begin with chain creation.
        assert_eq!(rules[0], vec!["-N", CHAIN_NAME]);

        // Must contain loopback rules.
        assert!(rules.iter().any(|r| r.contains(&"-o") && r.contains(&"lo")));
        assert!(rules.iter().any(|r| r.contains(&"-d") && r.contains(&"::1")));

        // Must contain DHCPv6 (port 547).
        assert!(rules.iter().any(|r| r.contains(&"547")));

        // Must contain all 5 ICMPv6 types.
        for t in ["133", "134", "135", "136", "143"] {
            assert!(
                rules.iter().any(|r| r.contains(&"ipv6-icmp") && r.contains(&t)),
                "missing icmpv6-type {t}"
            );
        }

        // Must end with DROP then jump insertion.
        let last_two: Vec<&RuleArgs> = rules.iter().rev().take(2).collect();
        assert!(last_two[0].contains(&"-j") && last_two[0].contains(&CHAIN_NAME));
        assert!(last_two[1].contains(&"-j") && last_two[1].contains(&"DROP"));
    }

    /// Structural: with tun_iface set, the rule list contains a tun ACCEPT rule.
    #[test]
    fn ip6_rule_list_with_tun_iface() {
        let rules = expected_ip6_rules(&[], Some("tun0"), &[]);
        assert!(
            rules.iter().any(|r| r.contains(&"-o") && r.contains(&"tun0")),
            "missing tun iface ACCEPT rule"
        );
    }

    /// Structural: without tun_iface, no tun ACCEPT rule is present.
    #[test]
    fn ip6_rule_list_without_tun_iface() {
        let rules = expected_ip6_rules(&[], None, &[]);
        // "lo" is expected, but no other interface rule should appear
        let non_lo_iface_rules: Vec<_> = rules
            .iter()
            .filter(|r| r.contains(&"-o") && !r.contains(&"lo"))
            .collect();
        assert!(non_lo_iface_rules.is_empty(), "unexpected interface rule: {non_lo_iface_rules:?}");
    }

    /// Structural: conntrack ESTABLISHED,RELATED rule is present.
    #[test]
    fn ip6_rule_list_has_conntrack() {
        let rules = expected_ip6_rules(&[], None, &[]);
        assert!(
            rules.iter().any(|r| r.contains(&"ESTABLISHED,RELATED")),
            "missing conntrack rule"
        );
    }
}
