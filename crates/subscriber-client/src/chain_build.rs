//! Translate a validated [`EntranceArtefact`](crate::artefact::EntranceArtefact)
//! into the [`ChainConfig`] shapes `mirage-client-core`'s `TunnelPool` /
//! `ChainSet` already consume — one single-hop chain per published entrance
//! face, exactly the "operator escape hatch" shape
//! `docs/mirage/plans/thin-customer-connector-2026-09-17.md` §2.2 names:
//! `HopConfig.public_key = Some(hex)` used as-is, no `PeerTable` lookup ever
//! reached.

use mirage_core::config::{
    ChainConfig, HandshakeStrategy, HopConfig, HopRole, ProtocolId, ReplyMode, TcpProtocolId,
    UdpProtocolId,
};
use mirage_core::crypto::AeadSuite;
use mirage_core::mirage::wire::RelayMode;

use crate::artefact::ArtefactEntrance;

/// Deterministic chain id for the `index`-th entrance, matching the plan's
/// own naming (§5: `chain_id = format!("entrance-{i}")`).
pub fn entrance_chain_id(index: usize) -> String {
    format!("entrance-{index}")
}

/// Build one single-hop [`ChainConfig`] for `entrance`.
///
/// `hop.public_key = Some(entrance.server_pubkey_hex)` and
/// `hop.camouflage = Some(entrance.camouflage)` are both explicit — together
/// they keep `mirage_control_plane::chain_uses_onion_framing` off the onion
/// path (it only turns onion-shaped for an explicit-camouflage single hop
/// when NO camouflage is given and a `peer_table` chain-face lookup
/// resolves one; an explicit `Some(_)` camouflage short-circuits that
/// entirely — `mirage2web/crates/mirage-dial-core/src/tunnel.rs:493-507`),
/// so every dial through this chain takes `establish_raw_tunnel`'s
/// single-hop direct branch: no peer_table lookup, no multi-hop shape, ever
/// reachable from a chain this function builds.
///
/// `relay_mode: RelayMode::OnionPeel` is set because it is the ONLY variant
/// the type has (`mirage_core::mirage::wire::RelayMode` — see its own module
/// doc for why the other two were removed 2026-09-02); it does not imply
/// onion routing by itself, as explained above.
///
/// # TCP vs UDP protocol
///
/// `entrance.camouflage.is_udp()` decides `protocol`: any of the seven
/// datagram-shaped [`TransportCamouflage`](mirage_core::config::TransportCamouflage)
/// variants (`Quic`, `Http3`, `Hysteria2`, `WireGuard`, `RawUdp`,
/// `WebTransport`, `DnsTunnel`) produces `ProtocolId::Udp(MirageUdp)`;
/// everything else produces the TCP protocol this crate has always built.
/// This function never dials — it only shapes the config. Only
/// `TransportCamouflage::RawUdp` is actually dialable by this crate's UDP
/// path today; see `crate::udp_dial`'s own module doc for why the other six
/// are recognised here but refused at dial time.
pub fn build_chain_config(index: usize, entrance: &ArtefactEntrance) -> ChainConfig {
    let protocol = if entrance.camouflage.is_udp() {
        ProtocolId::Udp(UdpProtocolId::MirageUdp)
    } else {
        ProtocolId::Tcp(TcpProtocolId::Mirage)
    };
    let hop = HopConfig {
        addr: entrance.addr.clone(),
        public_key: Some(entrance.server_pubkey_hex.clone()),
        node_id: None,
        sni: None,
        role: HopRole::Exit,
        display_name: None,
        camouflage: Some(entrance.camouflage.clone()),
        byte_cipher: None,
        camo_server_pubkey: None,
        selector_token_hex: None,
        relay_mode: RelayMode::OnionPeel,
    };
    ChainConfig {
        chain_id: entrance_chain_id(index),
        hops: vec![hop],
        handshake: HandshakeStrategy::Telescopic,
        reply: ReplyMode::Raw,
        protocol,
        priority: 0,
        shaping: Default::default(),
        camouflage: Default::default(),
        cover_traffic: Default::default(),
        socks5_bind: None,
        aead_suite: AeadSuite::ChaCha20Poly1305,
        kill_switch: None,
        group: None,
        nature: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_core::config::TransportCamouflage;

    fn entrance(addr: &str, camouflage: TransportCamouflage) -> ArtefactEntrance {
        ArtefactEntrance {
            addr: addr.to_owned(),
            server_pubkey_hex: "22".repeat(32),
            camouflage,
        }
    }

    // ── one chain per entrance, each with its own camouflage ────────────────

    #[test]
    fn one_single_hop_chain_per_entrance_with_its_own_camouflage() {
        let e0 = entrance("203.0.113.10:8443", TransportCamouflage::None);
        let e1 = entrance(
            "203.0.113.11:8443",
            TransportCamouflage::WebSocket { path: "/ws".to_owned(), host: "cdn.example.com".to_owned() },
        );

        let c0 = build_chain_config(0, &e0);
        let c1 = build_chain_config(1, &e1);

        assert_eq!(c0.chain_id, "entrance-0");
        assert_eq!(c1.chain_id, "entrance-1");
        assert_ne!(c0.chain_id, c1.chain_id);

        assert_eq!(c0.hops.len(), 1, "exactly one hop — never a chain");
        assert_eq!(c1.hops.len(), 1);

        assert_eq!(c0.hops[0].camouflage, Some(TransportCamouflage::None));
        assert_eq!(
            c1.hops[0].camouflage,
            Some(TransportCamouflage::WebSocket {
                path: "/ws".to_owned(),
                host: "cdn.example.com".to_owned(),
            })
        );

        assert_eq!(c0.hops[0].addr, "203.0.113.10:8443");
        assert_eq!(c1.hops[0].addr, "203.0.113.11:8443");
    }

    #[test]
    fn hop_public_key_is_the_entrance_server_pubkey_used_as_is() {
        let e = entrance("203.0.113.10:8443", TransportCamouflage::None);
        let c = build_chain_config(0, &e);
        assert_eq!(c.hops[0].public_key.as_deref(), Some(e.server_pubkey_hex.as_str()));
        assert_eq!(c.hops[0].node_id, None, "no peer_table lookup — explicit key only");
    }

    // ── TCP vs UDP protocol selection ───────────────────────────────────────

    #[test]
    fn raw_udp_camouflage_builds_a_udp_protocol_chain_not_tcp() {
        let e = entrance(
            "172.86.76.34:853",
            TransportCamouflage::RawUdp { bind_addr: "0.0.0.0:853".parse().expect("valid addr") },
        );
        let c = build_chain_config(1, &e);
        assert_eq!(
            c.protocol,
            ProtocolId::Udp(mirage_core::config::UdpProtocolId::MirageUdp),
            "a raw_udp entrance must build a UDP-protocol chain, never the hardcoded TCP one"
        );
        assert_eq!(c.chain_id, "entrance-1");
        assert_eq!(c.hops[0].camouflage, Some(e.camouflage.clone()));
    }

    #[test]
    fn none_and_websocket_camouflage_still_build_tcp_protocol_chains() {
        let e_none = entrance("203.0.113.10:8443", TransportCamouflage::None);
        let e_ws = entrance(
            "203.0.113.11:8443",
            TransportCamouflage::WebSocket { path: "/ws".to_owned(), host: "cdn.example.com".to_owned() },
        );
        assert_eq!(build_chain_config(0, &e_none).protocol, ProtocolId::Tcp(TcpProtocolId::Mirage));
        assert_eq!(build_chain_config(1, &e_ws).protocol, ProtocolId::Tcp(TcpProtocolId::Mirage));
    }

    /// Every other datagram-shaped camouflage `TransportCamouflage::is_udp()`
    /// recognises also routes to the UDP protocol here — this function only
    /// SHAPES the config; whether the dial path can actually speak that
    /// specific wrapper is `crate::udp_dial::UdpEntranceHandle::is_dialable`'s
    /// decision, not this one's. See `build_chain_config`'s own doc.
    #[test]
    fn every_udp_shaped_camouflage_builds_a_udp_protocol_chain() {
        let cases = vec![
            TransportCamouflage::Quic { alpn: vec!["h3".into()], bind_addr: "0.0.0.0:443".parse().expect("addr") },
            TransportCamouflage::Http3 {
                authority: "example.com".into(),
                tunnel_path: "/t".into(),
                alpn: "h3".into(),
                bind_addr: "0.0.0.0:443".parse().expect("addr"),
            },
            TransportCamouflage::Hysteria2 { psk_hex: "ab".into(), bind_addr: "0.0.0.0:443".parse().expect("addr") },
            TransportCamouflage::WireGuard {
                magic_byte: 1,
                junk_count: 2,
                junk_min: 10,
                junk_max: 20,
                bind_addr: "0.0.0.0:51820".parse().expect("addr"),
            },
            TransportCamouflage::WebTransport {
                path: "/wt".into(),
                authority: "example.com".into(),
                bind_addr: "0.0.0.0:443".parse().expect("addr"),
            },
            TransportCamouflage::DnsTunnel {
                domain: "example.com".into(),
                dns_server: "8.8.8.8:53".parse().expect("addr"),
                encoding: "base32".into(),
                query_type: "TXT".into(),
            },
        ];
        for camo in cases {
            let e = entrance("203.0.113.20:9000", camo.clone());
            let c = build_chain_config(0, &e);
            assert_eq!(
                c.protocol,
                ProtocolId::Udp(mirage_core::config::UdpProtocolId::MirageUdp),
                "{} must build a UDP-protocol chain",
                camo.label()
            );
        }
    }
}
