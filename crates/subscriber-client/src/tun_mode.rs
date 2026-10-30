//! TUN config this client writes for itself.
//!
//! Full: every destination not listed in `bypass.cidrs` goes to the local
//! SOCKS chain. Split: traffic stays direct, and only the user's services
//! go to that chain. `relay_addrs` stays empty — the carrier socket is
//! pinned by the dialer, not by a route around the entrance address.

use std::fs::File;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value};

use crate::scope::tun_service_action;

pub const CHAIN_NAME: &str = "edge";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunMode {
    Full,
    Split,
}

fn tun_device_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "utun"
    } else {
        "tun0"
    }
}

/// Policy document. Bypass CIDRs are NOT repeated here: mirage2tun appends
/// `bypass.cidrs` itself, and CIDR rules are first-match.
///
/// Split inclusions are the same lists SOCKS uses: a site rule or an app
/// rule sends that flow to the chain. Anything else stays direct.
pub fn policy_json(
    mode: TunMode,
    services: &[String],
    apps: &[String],
    bypass_services: &[String],
    bypass_apps: &[String],
) -> Result<Value, String> {
    match mode {
        TunMode::Full => {
            if !services.is_empty() || !apps.is_empty() {
                return Err("tun full does not take a site or an app".into());
            }
            let mut rules = Vec::new();
            push_named(&mut rules, bypass_services, bypass_apps, "bypass")?;
            Ok(json!({
                "default_action": format!("chain:{CHAIN_NAME}"),
                "rules": rules,
            }))
        }
        TunMode::Split => {
            if services.is_empty() && apps.is_empty() {
                return Err("tun split needs a site or an app".into());
            }
            let mut rules = Vec::new();
            push_named(&mut rules, bypass_services, bypass_apps, "bypass")?;
            push_named(&mut rules, services, apps, &format!("chain:{CHAIN_NAME}"))?;
            Ok(json!({
                "default_action": "bypass",
                "rules": rules,
            }))
        }
    }
}

fn push_named(rules: &mut Vec<Value>, services: &[String], apps: &[String], action: &str) -> Result<(), String> {
    for spec in services {
        let (kind, value) = tun_service_action(spec)?;
        let rule = if kind == "dest_cidr" {
            json!({"type": "dest_cidr", "cidr": value, "action": action})
        } else {
            json!({"type": "dest_domain", "domain": value, "action": action})
        };
        rules.push(rule);
    }
    for spec in apps {
        let path = spec.trim().to_ascii_lowercase().replace('\\', "/");
        if path.is_empty() || path.ends_with('/') {
            return Err(format!("empty app in --app {spec}"));
        }
        rules.push(json!({
            "type": "dest_app_path",
            "path": path,
            "action": action,
        }));
    }
    Ok(())
}

pub fn write_config(
    path: &Path,
    socks_bind: SocketAddr,
    bypass_cidrs: &[String],
    mode: TunMode,
    services: &[String],
    apps: &[String],
    bypass_services: &[String],
    bypass_apps: &[String],
    relay_addrs: &[String],
) -> Result<(), String> {
    let policy = policy_json(mode, services, apps, bypass_services, bypass_apps)?;
    let doc = json!({
        "tun_name": tun_device_name(),
        "tun_addr": "10.77.0.1/24",
        // The path MTU, not QUIC's floor. QUIC wants an Initial datagram of
        // at least 1200 bytes (RFC 9000, section 14.1) -- that is a MINIMUM.
        // A real client pads its Initial to the path MTU minus the IP and UDP
        // headers: Chrome sends 1252 bytes, and with an adapter MTU of 1200
        // that datagram did not fit the adapter at all. 1500 is the ordinary
        // Ethernet MTU; the chain carries whole IP packets, so nothing above
        // it needs to fit.
        "mtu": 1500,
        "chains": [{
            "name": CHAIN_NAME,
            "socks5_addr": socks_bind.to_string(),
        }],
        "bypass": { "cidrs": bypass_cidrs },
        "policy": policy,
        "relay_addrs": relay_addrs,
    });
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    let mut file = File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

pub fn spawn(bin: &Path, config: &Path) -> io::Result<Child> {
    let log_path = config.with_file_name("mirage-tun.log");
    let log = File::options().create(true).append(true).open(&log_path)?;
    let err = log.try_clone()?;
    let mut cmd = Command::new(bin);
    #[cfg(windows)]
    {
        let dir = bin.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "mirage2tun has no directory")
        })?;
        let target = dir.join("tun.json");
        // The device reads `tun.json` beside itself. When the config already
        // lives there, copying it onto itself would fail with a sharing
        // violation — the copy is skipped instead.
        let same = match (config.canonicalize(), target.canonicalize()) {
            (Ok(left), Ok(right)) => left == right,
            _ => config == &target,
        };
        if !same {
            std::fs::copy(config, &target)?;
        }
        cmd.arg("debug-run");
    }
    #[cfg(not(windows))]
    {
        cmd.arg("--config").arg(config).arg("--log-level").arg("info");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_policy_has_no_service_rules_and_no_relay() {
        let policy = policy_json(TunMode::Full, &[], &[], &[], &[]).unwrap();
        assert_eq!(policy["default_action"], "chain:edge");
        assert!(policy["rules"].as_array().unwrap().is_empty());
        let dir = std::env::temp_dir().join(format!("mirage-tun-cfg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tun.json");
        write_config(&path, "127.0.0.1:1080".parse().unwrap(), &["192.168.0.0/21".into()], TunMode::Full, &[], &[], &[], &[], &[])
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"relay_addrs\": []"));
        assert!(!text.contains("144.172"));
        assert!(text.contains("192.168.0.0/21"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_policy_defaults_to_bypass() {
        let policy = policy_json(
            TunMode::Split,
            &["1.1.1.1:443".into(), "api.ipify.org".into()],
            &["chrome.exe".into()],
            &["direct.example".into()],
            &["notepad.exe".into()],
        )
        .unwrap();
        assert_eq!(policy["default_action"], "bypass");
        let rules = policy["rules"].as_array().unwrap();
        assert_eq!(rules[0]["type"], "dest_domain");
        assert_eq!(rules[0]["domain"], "direct.example");
        assert_eq!(rules[0]["action"], "bypass");
        assert_eq!(rules[1]["type"], "dest_app_path");
        assert_eq!(rules[1]["path"], "notepad.exe");
        assert_eq!(rules[1]["action"], "bypass");
        assert_eq!(rules[2]["type"], "dest_cidr");
        assert_eq!(rules[2]["action"], "chain:edge");
        assert_eq!(rules[4]["type"], "dest_app_path");
        assert_eq!(rules[4]["path"], "chrome.exe");
        assert_eq!(rules[4]["action"], "chain:edge");
    }
}
