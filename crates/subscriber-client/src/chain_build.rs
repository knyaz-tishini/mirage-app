//! Translate a validated [`EntranceArtefact`](crate::artefact::EntranceArtefact)
//! into the [`ChainConfig`] shapes `mirage-client-core`'s `TunnelPool` /
//! `ChainSet` already consume — one single-hop chain per published entrance
//! face, exactly the "operator escape hatch" shape
//! `docs/mirage/plans/thin-customer-connector-2026-09-17.md` §2.2 names:
//! `HopConfig.public_key = Some(hex)` used as-is, no `PeerTable` lookup ever
//! reached.

use mirage_core::config::{
    ChainConfig, HandshakeStrategy, HopConfig, HopRole, ProtocolId, ReplyMode, TcpProtocolId,
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
pub fn build_chain_config(index: usize, entrance: &ArtefactEntrance) -> ChainConfig {
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
        protocol: ProtocolId::Tcp(TcpProtocolId::Mirage),
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
}
