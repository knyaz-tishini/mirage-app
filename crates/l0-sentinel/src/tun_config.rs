//! Auto-generation of tun.json from materialised chain config.
//!
//! Writes a complete macOS tun `BinConfig`-compatible JSON file. Managed fields
//! (`relay_addrs`, `chains`) are always overwritten from live chain data.
//! Defaults (`tun_addr`, `mtu`, etc.) are inserted only when absent so a user
//! template can override them.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mirage_core::config::ChainConfig;
use tracing::{info, warn};

use crate::MANAGED_BY_VALUE;

/// Private/LAN ranges that must egress DIRECT (real NIC), never the tunnel —
/// so SSH and local services survive when the tun default-routes everything.
const LAN_BYPASS_CIDRS: &[&str] =
    &["192.168.0.0/16", "172.16.0.0/12", "10.0.0.0/8", "169.254.0.0/16"];

// ── Public API ────────────────────────────────────────────────────────────────

/// Write (or update) the tun.json at `path`.
///
/// Produces a complete macOS tun `BinConfig`-compatible JSON:
/// - Always managed: `relay_addrs`, `chains`, `_managed_by`.
/// - Insert-if-absent (template overrides): `tun_addr`, `mtu`, `kill_switch`,
///   `chain_dns`, `dns_upstream`, `api_bind`, `bypass`.
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

    // ── Managed: always overwritten from live chain data ──────────────────────

    // Collect relay IPs (host part of every hop addr, deduped).
    let (v4_addrs, v6_addrs) = extract_relay_addrs(chains);
    let n_v4 = v4_addrs.len();
    let n_v6 = v6_addrs.len();

    let mut relay_vals: Vec<serde_json::Value> = v4_addrs
        .into_iter()
        .chain(v6_addrs)
        .map(|ip| serde_json::Value::String(ip.to_string()))
        .collect();
    relay_vals.sort_by(|a, b| a.as_str().cmp(&b.as_str()));

    map.insert("relay_addrs".to_owned(), serde_json::Value::Array(relay_vals));

    // Build chains array: one entry per ChainConfig with a deterministic
    // socks5_addr (127.0.0.1:{19480 + index}). Uses socks5_bind when present
    // so the value matches what materialise() injected into client-config.json.
    let chains_val: Vec<serde_json::Value> = chains
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let socks5_addr = c
                .socks5_bind
                .clone()
                .unwrap_or_else(|| format!("127.0.0.1:{}", 19480u16 + i as u16));
            serde_json::json!({
                "name": c.chain_id,
                "socks5_addr": socks5_addr,
            })
        })
        .collect();
    let n_chains = chains_val.len();
    map.insert("chains".to_owned(), serde_json::Value::Array(chains_val));

    map.insert(
        "_managed_by".to_owned(),
        serde_json::Value::String(MANAGED_BY_VALUE.to_owned()),
    );

    // ── Insert-if-absent: template can override these ─────────────────────────

    if !map.contains_key("tun_addr") {
        map.insert("tun_addr".to_owned(), serde_json::json!("10.255.0.2/24"));
    }
    if !map.contains_key("mtu") {
        map.insert("mtu".to_owned(), serde_json::json!(1280u16));
    }
    if !map.contains_key("kill_switch") {
        map.insert("kill_switch".to_owned(), serde_json::json!(false));
    }
    if !map.contains_key("chain_dns") {
        map.insert("chain_dns".to_owned(), serde_json::json!([]));
    }
    if !map.contains_key("dns_upstream") {
        map.insert("dns_upstream".to_owned(), serde_json::json!("8.8.8.8:53"));
    }
    if !map.contains_key("api_bind") {
        map.insert("api_bind".to_owned(), serde_json::json!("127.0.0.1:19485"));
    }
    if !map.contains_key("bypass") {
        let cidrs: Vec<serde_json::Value> = LAN_BYPASS_CIDRS
            .iter()
            .map(|s| serde_json::Value::String(s.to_string()))
            .collect();
        map.insert(
            "bypass".to_owned(),
            serde_json::json!({"cidrs": cidrs, "domains": []}),
        );
    }

    let content =
        serde_json::to_string_pretty(&obj).context("failed to serialise tun config")?;
    crate::write_atomic(path, &content)
        .with_context(|| format!("failed to write tun config {:?}", path))?;

    info!(
        "[sentinel] wrote tun config {:?} (chains: {}, relay_addrs: {} v4, {} v6)",
        path, n_chains, n_v4, n_v6
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
/// Source: `hops[*].addr` (host:port string — parse host part only).
/// Phase 8+ removed `l1` / `l2` legacy fields; hops carry every node now.
pub fn extract_relay_addrs(chains: &[ChainConfig]) -> (BTreeSet<IpAddr>, BTreeSet<IpAddr>) {
    let mut v4: BTreeSet<IpAddr> = BTreeSet::new();
    let mut v6: BTreeSet<IpAddr> = BTreeSet::new();

    for chain in chains {
        for hop in &chain.hops {
            // `hop.addr` is "host:port" string.
            if let Ok(sa) = hop.addr.parse::<std::net::SocketAddr>() {
                insert_ip(sa.ip(), &mut v4, &mut v6);
            }
        }
    }

    (v4, v6)
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
        CamouflageMode, ChainConfig, CoverTrafficConfig, HandshakeStrategy,
        HopConfig, HopRole, ProtocolId, ReplyMode, ShapingPreset, TcpProtocolId,
    };
    use std::net::{IpAddr, Ipv4Addr};

    fn make_hop(addr: &str, role: HopRole) -> HopConfig {
        HopConfig {
            addr: addr.into(),
            public_key: Some("aa".repeat(32)),
            node_id: None,
            sni: None,
            role,
            display_name: None,
            camouflage: None,
            byte_cipher: None,
            camo_server_pubkey: None,
            relay_mode: mirage_core::mirage::wire::RelayMode::OnionPeel,
        }
    }

    fn make_chain_hops(hops: Vec<HopConfig>) -> ChainConfig {
        ChainConfig {
            chain_id: "test-hops".into(),
            hops,
            handshake: HandshakeStrategy::default(),
            reply: ReplyMode::default(),
            protocol: ProtocolId::Tcp(TcpProtocolId::Mirage),
            priority: 0,
            shaping: ShapingPreset::default(),
            camouflage: CamouflageMode::default(),
            cover_traffic: CoverTrafficConfig::default(),
            socks5_bind: None,
            websocket_xray: None,
            shadowtls_v3: None,
            vless_reality: None,
            udp_hysteria2_native: None,
            aead_suite: mirage_core::crypto::AeadSuite::ChaCha20Poly1305,
            kill_switch: None,
            group: None,
        }
    }

    #[test]
    fn extract_relay_addrs_from_chain_config() {
        let chain = make_chain_hops(vec![
            make_hop("91.218.245.247:8443", HopRole::Entry),
            make_hop("45.59.119.139:8443", HopRole::Exit),
        ]);

        let (v4, _v6) = extract_relay_addrs(&[chain]);

        assert!(v4.contains(&IpAddr::V4(Ipv4Addr::new(91, 218, 245, 247))));
        assert!(v4.contains(&IpAddr::V4(Ipv4Addr::new(45, 59, 119, 139))));
    }

    #[test]
    fn extract_relay_addrs_deduplication() {
        let chain = make_chain_hops(vec![
            make_hop("91.218.245.247:8443", HopRole::Entry),
            make_hop("91.218.245.247:9443", HopRole::Exit),
        ]);

        let (v4, _v6) = extract_relay_addrs(&[chain]);
        assert_eq!(v4.len(), 1, "duplicate IP must be deduped");
    }

    #[test]
    fn extract_relay_addrs_hops() {
        let chain = make_chain_hops(vec![make_hop("10.0.0.1:1234", HopRole::Entry)]);
        let (v4, _v6) = extract_relay_addrs(&[chain]);
        assert!(v4.contains(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    }

    #[test]
    fn tun_config_writes_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tun.json").to_path_buf();
        let chain = make_chain_hops(vec![
            make_hop("1.2.3.4:8443", HopRole::Entry),
            make_hop("5.6.7.8:8443", HopRole::Exit),
        ]);

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

        let chain = make_chain_hops(vec![
            make_hop("1.2.3.4:8443", HopRole::Entry),
            make_hop("5.6.7.8:8443", HopRole::Exit),
        ]);

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
        // socks5_addr is a top-level template key (not inside chains[]) so it must
        // be preserved by the insert-if-absent logic.
        std::fs::write(
            &tpl_path,
            r#"{"relay_addrs":["100.100.100.100"],"socks5_addr":"127.0.0.1:19480"}"#,
        )
        .unwrap();
        let out_path = dir.path().join("tun.json").to_path_buf();

        let chain = make_chain_hops(vec![
            make_hop("1.2.3.4:8443", HopRole::Entry),
            make_hop("5.6.7.8:8443", HopRole::Exit),
        ]);

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
        // socks5_addr from template must be preserved (top-level key, not managed).
        assert_eq!(v["socks5_addr"].as_str(), Some("127.0.0.1:19480"));
    }

    #[test]
    fn tun_config_malformed_template_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let tpl_path = dir.path().join("bad-tun-template.json");
        std::fs::write(&tpl_path, "this is not json{{{{").unwrap();
        let out_path = dir.path().join("tun.json").to_path_buf();

        let chain = make_chain_hops(vec![
            make_hop("1.2.3.4:8443", HopRole::Entry),
            make_hop("5.6.7.8:8443", HopRole::Exit),
        ]);

        let result = write_tun_config(&[chain], &out_path, Some(tpl_path.as_path()), true);
        assert!(result.is_err(), "malformed template must return error");
        assert!(!out_path.exists(), "output file must not be created on template error");
    }

    #[test]
    fn tun_config_complete_binconfig_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tun.json").to_path_buf();

        // Single chain with socks5_bind: None — defaults to 127.0.0.1:19480.
        let mut chain = make_chain_hops(vec![make_hop("1.2.3.4:8443", HopRole::Entry)]);
        chain.chain_id = "chain-abc".into();
        chain.socks5_bind = None;

        write_tun_config(&[chain], &path, None, true).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(v["tun_addr"].as_str(), Some("10.255.0.2/24"));
        assert_eq!(v["mtu"].as_u64(), Some(1280));
        assert_eq!(v["kill_switch"].as_bool(), Some(false));
        assert_eq!(v["dns_upstream"].as_str(), Some("8.8.8.8:53"));
        assert_eq!(v["api_bind"].as_str(), Some("127.0.0.1:19485"));

        let chains = v["chains"].as_array().unwrap();
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0]["name"].as_str(), Some("chain-abc"));
        assert_eq!(chains[0]["socks5_addr"].as_str(), Some("127.0.0.1:19480"));

        let bypass = &v["bypass"];
        assert!(bypass.is_object(), "bypass must be an object");
        let cidrs = bypass["cidrs"].as_array().unwrap();
        assert!(
            cidrs.iter().any(|c| c.as_str() == Some("192.168.0.0/16")),
            "bypass must contain LAN CIDR"
        );
    }
}
