//! TUN config this client writes for itself.
//!
//! Full: every destination not listed in `bypass.cidrs` goes to the local
//! SOCKS chain. Split: traffic stays direct, and only the user's services
//! go to that chain. `relay_addrs` stays empty — the carrier socket is
//! pinned by the dialer, not by a route around the entrance address.
//!
//! The datapath is a separate choice, [`WireMode`]: `stack` is the byte
//! stream path (the device terminates TCP and UDP in userspace), `raw_ip`
//! is the mono-protocol path (the device hands whole IP packets over the
//! raw-IP SOCKS5 command, and they ride the QUIC datagrams of the edge
//! session).

use std::fs::File;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use mirage_core::mirage::wire::Protocol;
use serde_json::{json, Value};

use crate::artefact::{ArtefactEntrance, DeliveryArtefact};
use crate::scope::tun_service_action;

pub const CHAIN_NAME: &str = "edge";

/// The tunnel address the client falls back to when the box assigns none.
///
/// A box of the older wire versions sends no address: the client keeps the
/// address it always had. The box of the packet plane assigns every client
/// its own address and sends it in the welcome.
pub const DEFAULT_TUN_ADDR: &str = "10.77.0.1/24";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunMode {
    Full,
    Split,
}

/// The datapath the tunnel device speaks to this client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireMode {
    /// The device terminates TCP and UDP in userspace and hands byte streams
    /// over the SOCKS5 proxy.
    Stack,
    /// The device hands whole IP packets over the raw-IP SOCKS5 command, and
    /// this client tunnels them over the QUIC datagrams of the edge session.
    RawIp,
}

impl WireMode {
    /// The name the device's config carries.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stack => "stack",
            Self::RawIp => "raw_ip",
        }
    }

    /// The adapter MTU this mode needs.
    ///
    /// The raw-IP mode carries whole IP packets in QUIC datagrams, and a
    /// datagram holds at most [`mirage_protocol::plane::MAX_PACKET_SIZE`]
    /// bytes: an adapter MTU above that has the device drop packets that
    /// could never fit the plane.
    pub fn mtu(&self) -> u16 {
        match self {
            Self::Stack => 1500,
            Self::RawIp => mirage_protocol::plane::MAX_PACKET_SIZE as u16,
        }
    }

    /// The mode the given faces call for.
    ///
    /// A QUIC face is the mono-protocol plane: the whole IP packets ride
    /// its datagrams. Every other face keeps the byte-stream path.
    pub fn for_entrances(entrances: &[ArtefactEntrance]) -> Self {
        let plane = entrances
            .iter()
            .any(|entrance| matches!(entrance.protocol, Protocol::Quic { .. }));
        if plane {
            Self::RawIp
        } else {
            Self::Stack
        }
    }
}

/// The mode the given artefact's faces call for.
///
/// An onion artefact names a chain, not a box: a chain has no packet plane,
/// so it keeps the byte-stream path.
pub fn wire_mode_for(artefact: &DeliveryArtefact) -> WireMode {
    match artefact {
        DeliveryArtefact::Vpn(vpn) => WireMode::for_entrances(&vpn.entrances),
        DeliveryArtefact::Onion(_) => WireMode::Stack,
    }
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

/// Write the device config.
///
/// `tun_addr` is the address the tunnel adapter carries, in CIDR form. It is
/// the address the box assigned to this client ("10.77.0.7/24"), and the
/// adapter must carry exactly it: the box drops the packets of a peer that
/// sends from another address. A box that assigns nothing gets
/// [`DEFAULT_TUN_ADDR`].
pub fn write_config(
    path: &Path,
    socks_bind: SocketAddr,
    tun_addr: &str,
    bypass_cidrs: &[String],
    mode: TunMode,
    wire: WireMode,
    services: &[String],
    apps: &[String],
    bypass_services: &[String],
    bypass_apps: &[String],
    relay_addrs: &[String],
) -> Result<(), String> {
    let policy = policy_json(mode, services, apps, bypass_services, bypass_apps)?;
    let mut chain = json!({
        "name": CHAIN_NAME,
        "socks5_addr": socks_bind.to_string(),
    });
    if wire == WireMode::RawIp {
        // The raw-IP chain carries whole IP packets, not byte streams.
        chain["protocol"] = json!("raw_ip");
    }
    let doc = json!({
        "tun_name": tun_device_name(),
        "tun_addr": tun_addr,
        // The datapath: `stack` is the byte-stream path, `raw_ip` is the
        // mono-protocol path.
        "mode": wire.as_str(),
        // The path MTU, not QUIC's floor. QUIC wants an Initial datagram of
        // at least 1200 bytes (RFC 9000, section 14.1) -- that is a MINIMUM.
        // A real client pads its Initial to the path MTU minus the IP and UDP
        // headers: Chrome sends 1252 bytes, and with an adapter MTU of 1200
        // that datagram did not fit the adapter at all. 1500 is the ordinary
        // Ethernet MTU; the chain carries whole IP packets, so nothing above
        // it needs to fit. The raw-IP mode carries the packets in QUIC
        // datagrams, so its MTU is the size of one datagram.
        "mtu": wire.mtu(),
        "chains": [chain],
        "bypass": { "cidrs": bypass_cidrs },
        // The applications that leave the tunnel.  The policy rule above is
        // the network-level way of saying it; this is the per-application
        // way, done by the split-tunnel driver: the image keeps the physical
        // interface while everything else goes through the tunnel.
        "split_tunnel_apps": bypass_apps,
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
        write_config(
            &path,
            "127.0.0.1:1080".parse().unwrap(),
            DEFAULT_TUN_ADDR,
            &["192.168.0.0/21".into()],
            TunMode::Full,
            WireMode::Stack,
            &[],
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"relay_addrs\": []"));
        assert!(!text.contains("144.172"));
        assert!(text.contains("192.168.0.0/21"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_raw_ip_mode_writes_the_datagram_mtu_and_the_raw_chain() {
        let dir = std::env::temp_dir().join(format!("mirage-tun-cfg-raw-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tun.json");
        write_config(
            &path,
            "127.0.0.1:1080".parse().unwrap(),
            "10.77.0.7/24",
            &[],
            TunMode::Full,
            WireMode::RawIp,
            &[],
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["mode"], "raw_ip");
        assert_eq!(doc["mtu"], 1280);
        // The adapter carries the address the box assigned.
        assert_eq!(doc["tun_addr"], "10.77.0.7/24");
        assert_eq!(doc["chains"][0]["protocol"], "raw_ip");
        assert_eq!(doc["chains"][0]["name"], CHAIN_NAME);
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
