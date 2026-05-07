//! Auto-generation of tun.json `relay_addrs` from materialised chain config.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mirage_core::config::{ChainConfig, ChainNode};
use tracing::{info, warn};

use crate::MANAGED_BY_VALUE;

// ── Public API ────────────────────────────────────────────────────────────────

/// Write (or update) the tun.json at `path`.
///
/// - Collects every v4/v6 IP from `l1`, `l2`, and `hops[*].addr` across all chains.
/// - Loads the template from `template_path` (or uses a minimal stub when `None`).
/// - Injects `relay_addrs` + `_managed_by` marker.
/// - Atomic write (tmp + rename).
///
/// `respect_user_config`: when `true`, skips writing if the file exists without
/// the `_managed_by` marker — same semantics as the client config.
pub fn write_tun_config(
    chains: &[ChainConfig],
    path: &PathBuf,
    template_path: Option<&Path>,
    respect_user_config: bool,
) -> Result<()> {
    // Respect-user-config guard: if file exists and has no managed marker.
    if path.exists() {
        let has_marker = is_tun_managed(path);
        if !has_marker && respect_user_config {
            warn!(
                "[sentinel] tun config {:?} exists without managed marker — skipping overwrite",
                path
            );
            return Ok(());
        }
    }

    // Load template or use minimal stub.
    let mut obj: serde_json::Value = if let Some(tmpl) = template_path {
        let raw = std::fs::read_to_string(tmpl)
            .with_context(|| format!("cannot read tun template {:?}", tmpl))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("tun template {:?} is not valid JSON", tmpl))?
    } else {
        serde_json::json!({})
    };

    let map = obj
        .as_object_mut()
        .context("tun template must be a JSON object")?;

    // Collect relay IPs.
    let (v4_addrs, v6_addrs) = extract_relay_addrs(chains);
    let n_v4 = v4_addrs.len();
    let n_v6 = v6_addrs.len();

    let mut all: Vec<serde_json::Value> = v4_addrs
        .into_iter()
        .chain(v6_addrs)
        .map(|ip| serde_json::Value::String(ip.to_string()))
        .collect();
    // Sort for deterministic output.
    all.sort_by(|a, b| a.as_str().cmp(&b.as_str()));

    // Always overwrite relay_addrs (even if template had hardcoded ones).
    map.insert("relay_addrs".to_owned(), serde_json::Value::Array(all));
    map.insert(
        "_managed_by".to_owned(),
        serde_json::Value::String(MANAGED_BY_VALUE.to_owned()),
    );

    let content =
        serde_json::to_string_pretty(&obj).context("failed to serialise tun config")?;
    crate::write_atomic(path, &content)
        .with_context(|| format!("failed to write tun config {:?}", path))?;

    info!(
        "[sentinel] wrote tun config {:?} (relay_addrs: {} v4, {} v6)",
        path, n_v4, n_v6
    );
    Ok(())
}

/// Returns `true` if the file at `path` contains the sentinel managed marker.
fn is_tun_managed(path: &PathBuf) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
        return v.get("_managed_by").and_then(|v| v.as_str()) == Some(MANAGED_BY_VALUE);
    }
    false
}

// ── Relay-addr extraction ─────────────────────────────────────────────────────

/// Extract deduplicated (v4 set, v6 set) from all ChainConfig nodes.
///
/// Sources:
/// - `l1.addr` / `l1.addr_v6`
/// - `l2.addr` / `l2.addr_v6`
/// - `hops[*].addr` (host:port string — parse host part only)
pub fn extract_relay_addrs(chains: &[ChainConfig]) -> (BTreeSet<IpAddr>, BTreeSet<IpAddr>) {
    let mut v4: BTreeSet<IpAddr> = BTreeSet::new();
    let mut v6: BTreeSet<IpAddr> = BTreeSet::new();

    for chain in chains {
        if let Some(ref node) = chain.l1 {
            collect_node(node, &mut v4, &mut v6);
        }
        if let Some(ref node) = chain.l2 {
            collect_node(node, &mut v4, &mut v6);
        }
        for hop in &chain.hops {
            // `hop.addr` is "host:port" string.
            if let Ok(sa) = hop.addr.parse::<std::net::SocketAddr>() {
                insert_ip(sa.ip(), &mut v4, &mut v6);
            }
        }
    }

    (v4, v6)
}

fn collect_node(node: &ChainNode, v4: &mut BTreeSet<IpAddr>, v6: &mut BTreeSet<IpAddr>) {
    insert_ip(node.addr.ip(), v4, v6);
    if let Some(sa6) = node.addr_v6 {
        insert_ip(sa6.ip(), v4, v6);
    }
}

fn insert_ip(ip: IpAddr, v4: &mut BTreeSet<IpAddr>, v6: &mut BTreeSet<IpAddr>) {
    match ip {
        IpAddr::V4(_) => { v4.insert(ip); }
        IpAddr::V6(_) => { v6.insert(ip); }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_core::config::{
        CamouflageMode, ChainConfig, ChainNode, CoverTrafficConfig, HandshakeStrategy,
        HopConfig, HopRole, ProtocolId, ReplyMode, ShapingPreset, TcpProtocolId,
    };
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn make_node(v4: &str, v6: Option<&str>) -> ChainNode {
        ChainNode {
            addr: v4.parse().unwrap(),
            addr_v6: v6.map(|s| s.parse().unwrap()),
            public_key: "aa".repeat(32),
            sni: None,
            display_name: None,
        }
    }

    fn make_chain_two_hop(l1: ChainNode, l2: ChainNode) -> ChainConfig {
        ChainConfig {
            chain_id: "test".into(),
            l1: Some(l1),
            l2: Some(l2),
            l1_transparent: false,
            hops: vec![],
            handshake: HandshakeStrategy::default(),
            reply: ReplyMode::default(),
            protocol: ProtocolId::Tcp(TcpProtocolId::Mirage),
            priority: 0,
            shaping: ShapingPreset::default(),
            camouflage: CamouflageMode::default(),
            cover_traffic: CoverTrafficConfig::default(),
            byte_cipher: None,
            socks5_bind: None,
        }
    }

    fn make_chain_hops(hops: Vec<HopConfig>) -> ChainConfig {
        ChainConfig {
            chain_id: "test-hops".into(),
            l1: None,
            l2: None,
            l1_transparent: false,
            hops,
            handshake: HandshakeStrategy::default(),
            reply: ReplyMode::default(),
            protocol: ProtocolId::Tcp(TcpProtocolId::Mirage),
            priority: 0,
            shaping: ShapingPreset::default(),
            camouflage: CamouflageMode::default(),
            cover_traffic: CoverTrafficConfig::default(),
            byte_cipher: None,
            socks5_bind: None,
        }
    }

    #[test]
    fn extract_relay_addrs_from_chain_config() {
        let l1 = make_node("91.218.245.247:8443", Some("[2a01:4f8::1]:8443"));
        let l2 = make_node("45.59.119.139:8443", None);
        let chain = make_chain_two_hop(l1, l2);

        let (v4, v6) = extract_relay_addrs(&[chain]);

        assert_eq!(v4.len(), 2, "expected 2 v4 IPs");
        assert_eq!(v6.len(), 1, "expected 1 v6 IP");
        assert!(v4.contains(&IpAddr::V4(Ipv4Addr::new(91, 218, 245, 247))));
        assert!(v4.contains(&IpAddr::V4(Ipv4Addr::new(45, 59, 119, 139))));
        assert!(v6.contains(&IpAddr::V6("2a01:4f8::1".parse::<Ipv6Addr>().unwrap())));
    }

    #[test]
    fn extract_relay_addrs_deduplication() {
        // Same IP in l1 and l2.
        let l1 = make_node("91.218.245.247:8443", None);
        let l2 = make_node("91.218.245.247:9443", None);
        let chain = make_chain_two_hop(l1, l2);

        let (v4, _v6) = extract_relay_addrs(&[chain]);
        assert_eq!(v4.len(), 1, "duplicate IP must be deduped");
    }

    #[test]
    fn extract_relay_addrs_hops() {
        let hop = HopConfig {
            addr: "10.0.0.1:1234".into(),
            public_key: "bb".repeat(32),
            sni: None,
            role: HopRole::Entry,
            display_name: None,
        };
        let chain = make_chain_hops(vec![hop]);
        let (v4, _v6) = extract_relay_addrs(&[chain]);
        assert!(v4.contains(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    }

    #[test]
    fn tun_config_writes_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tun.json").to_path_buf();
        let l1 = make_node("1.2.3.4:8443", None);
        let l2 = make_node("5.6.7.8:8443", None);
        let chain = make_chain_two_hop(l1, l2);

        write_tun_config(&[chain], &path, None, true).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v.get("_managed_by").and_then(|x| x.as_str()),
            Some(MANAGED_BY_VALUE)
        );
        let addrs = v["relay_addrs"].as_array().unwrap();
        assert_eq!(addrs.len(), 2);
    }

    #[test]
    fn tun_config_respects_user_marker_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tun.json").to_path_buf();

        // Write a user-managed file (no _managed_by).
        std::fs::write(&path, r#"{"relay_addrs":["9.9.9.9"]}"#).unwrap();

        let l1 = make_node("1.2.3.4:8443", None);
        let l2 = make_node("5.6.7.8:8443", None);
        let chain = make_chain_two_hop(l1, l2);

        // With respect_user_config=true: should not overwrite.
        write_tun_config(&[chain.clone()], &path, None, true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("9.9.9.9"),
            "user file must not be overwritten when marker absent"
        );

        // With respect_user_config=false: should overwrite.
        write_tun_config(&[chain], &path, None, false).unwrap();
        let raw2 = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw2.contains("9.9.9.9"),
            "file should be overwritten when respect_user_config=false"
        );
        assert!(raw2.contains(MANAGED_BY_VALUE));
    }

    #[test]
    fn tun_config_template_relay_addrs_overridden() {
        let dir = tempfile::tempdir().unwrap();
        let tpl_path = dir.path().join("tun-template.json");
        // Template has hardcoded relay_addrs — they should be replaced.
        std::fs::write(
            &tpl_path,
            r#"{"relay_addrs":["100.100.100.100"],"socks5_addr":"127.0.0.1:19480"}"#,
        )
        .unwrap();
        let out_path = dir.path().join("tun.json").to_path_buf();

        let l1 = make_node("1.2.3.4:8443", None);
        let l2 = make_node("5.6.7.8:8443", None);
        let chain = make_chain_two_hop(l1, l2);

        write_tun_config(&[chain], &out_path, Some(tpl_path.as_path()), true).unwrap();

        let raw = std::fs::read_to_string(&out_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let addrs = v["relay_addrs"].as_array().unwrap();
        // Must NOT contain the template's hardcoded 100.100.100.100.
        assert!(
            !addrs.iter().any(|x| x.as_str() == Some("100.100.100.100")),
            "template relay_addrs must be overridden"
        );
        // Must contain actual chain IPs.
        assert_eq!(addrs.len(), 2);
        // socks5_addr from template must be preserved.
        assert_eq!(v["socks5_addr"].as_str(), Some("127.0.0.1:19480"));
    }

    #[test]
    fn tun_config_malformed_template_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let tpl_path = dir.path().join("bad-tun-template.json");
        std::fs::write(&tpl_path, "this is not json{{{{").unwrap();
        let out_path = dir.path().join("tun.json").to_path_buf();

        let l1 = make_node("1.2.3.4:8443", None);
        let l2 = make_node("5.6.7.8:8443", None);
        let chain = make_chain_two_hop(l1, l2);

        let result = write_tun_config(&[chain], &out_path, Some(tpl_path.as_path()), true);
        assert!(result.is_err(), "malformed template must return error");
        assert!(!out_path.exists(), "output file must not be created on template error");
    }
}
