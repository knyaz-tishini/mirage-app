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
//! loopback and each IPv6 relay address, then drops the rest.  When no IPv6
//! relay addresses are present, all outbound IPv6 is dropped.

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
    /// Whether the rules are currently installed.
    active: bool,
}

impl KillSwitch {
    /// Create a new (inactive) kill switch allowing traffic to all addresses in
    /// `relay_addrs`.  Each unique (IP, port) pair gets its own ACCEPT rule.
    pub fn new(relay_addrs: Vec<SocketAddr>) -> Self {
        Self {
            relay_addrs,
            active: false,
        }
    }

    /// Install iptables rules.  Idempotent — safe to call when already active
    /// (it will attempt to create the chain; `iptables` will return a
    /// non-fatal "Chain already exists" error which we ignore).
    pub fn activate(&mut self) -> Result<(), KillSwitchError> {
        // Verify iptables is available.
        which_iptables()?;

        info!(
            relays = ?self.relay_addrs,
            "kill switch: activating (chain {}) — allowing {} relay address(es)",
            CHAIN_NAME,
            self.relay_addrs.len(),
        );

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

        // 3. Allow established/related (replies to connections we initiated).
        run_iptables(&[
            "-A", CHAIN_NAME,
            "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED",
            "-j", "ACCEPT",
        ])?;

        // 4. Allow TCP to each relay address (one rule per unique IP:port pair).
        for addr in &self.relay_addrs {
            let relay_ip = addr.ip().to_string();
            let relay_port = addr.port().to_string();
            info!(relay = %addr, "kill switch: adding ACCEPT rule for relay");
            run_iptables(&[
                "-A", CHAIN_NAME,
                "-d", &relay_ip,
                "-p", "tcp",
                "--dport", &relay_port,
                "-j", "ACCEPT",
            ])?;
        }

        // 5. Allow DNS to localhost (dns_forwarder listens on 127.0.0.1).
        run_iptables(&[
            "-A", CHAIN_NAME,
            "-d", "127.0.0.1",
            "-p", "udp",
            "--dport", "53",
            "-j", "ACCEPT",
        ])?;

        // 6. Drop everything else.
        run_iptables(&["-A", CHAIN_NAME, "-j", "DROP"])?;

        // 7. Insert jump at the front of OUTPUT.
        run_iptables(&["-I", "OUTPUT", "1", "-j", CHAIN_NAME])?;

        // 8. IPv6 kill-switch via a parallel MIRAGE_KS chain in ip6tables.
        install_ip6_chain(&self.relay_addrs);

        self.active = true;
        info!(
            relay_count = self.relay_addrs.len(),
            "kill switch: active — all traffic except {} relay(s) is blocked",
            self.relay_addrs.len(),
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

        // Remove jump from OUTPUT.
        if let Err(e) = run_iptables(&["-D", "OUTPUT", "-j", CHAIN_NAME]) {
            warn!("kill switch deactivate: remove jump failed: {e}");
        }

        // Flush all rules from the chain.
        if let Err(e) = run_iptables(&["-F", CHAIN_NAME]) {
            warn!("kill switch deactivate: flush chain failed: {e}");
        }

        // Delete the chain.
        if let Err(e) = run_iptables(&["-X", CHAIN_NAME]) {
            warn!("kill switch deactivate: delete chain failed: {e}");
        }

        // Remove the IPv6 MIRAGE_KS chain (tolerant — may not exist).
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

/// Install an `ip6tables` MIRAGE_KS chain that permits loopback and any IPv6
/// relay addresses, then drops the rest.
///
/// When no IPv6 relay addresses are in `addrs`, all outbound IPv6 is dropped.
/// All operations are tolerant — ip6tables may not exist on the host.
fn install_ip6_chain(addrs: &[SocketAddr]) {
    // Create chain (ignore "already exists").
    run_ip6tables_tolerant(&["-N", CHAIN_NAME]);

    // Loopback.
    run_ip6tables_tolerant(&["-A", CHAIN_NAME, "-o", "lo", "-j", "ACCEPT"]);
    run_ip6tables_tolerant(&["-A", CHAIN_NAME, "-d", "::1", "-j", "ACCEPT"]);

    // Established / related.
    run_ip6tables_tolerant(&[
        "-A", CHAIN_NAME,
        "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED",
        "-j", "ACCEPT",
    ]);

    // Per-relay IPv6 ACCEPT rules.
    for addr in addrs {
        if addr.is_ipv6() {
            let relay_ip = addr.ip().to_string();
            let relay_port = addr.port().to_string();
            info!(relay = %addr, "kill switch: adding ip6tables ACCEPT rule for v6 relay");
            run_ip6tables_tolerant(&[
                "-A", CHAIN_NAME,
                "-d", &relay_ip,
                "-p", "tcp",
                "--dport", &relay_port,
                "-j", "ACCEPT",
            ]);
        }
    }

    // Drop everything else.
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
            "10.0.0.1:4431".parse().unwrap(), // v4 — should be skipped by install_ip6_chain
        ];
        install_ip6_chain(&addrs);
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
}
