//! Two artefacts, one client: the VPN one and the onion one.
//!
//! The fleet's own artefact has no `kind` field and must keep parsing as the
//! VPN artefact — byte for byte, the shape the panel writes. The onion
//! artefact carries `"kind": "onion"` and names a chain instead of a box.

use mirage_subscriber_client::{DeliveryArtefact, EntranceArtefact, OnionArtefact};

/// The fleet's artefact, exactly as the panel writes it.
const FLEET_ARTEFACT: &str = r#"{
  "v": 1,
  "private_key_hex": "1111111111111111111111111111111111111111111111111111111111111111",
  "mesh_id": "MESH2",
  "entrances": [{
    "addr": "203.0.113.9:443",
    "server_pubkey_hex": "3333333333333333333333333333333333333333333333333333333333333333",
    "camouflage": {"type": "tls_real", "sni": "entrance.example.com", "alpn": ["h2"]}
  }],
  "mode": "full_auto",
  "label": "Ada",
  "issued_at_ms": 1726000000000
}"#;

/// The onion artefact: a circuit of two hops.
///
/// The hop's `node_id` is not decoration: the exit binds its credential check
/// to it, and the chain builder refuses a hop without one. `self_id` is the
/// client's own identity in the mesh, `identity_not_after_ms` the bound after
/// which the exit stops honouring the chain leg.
const ONION_ARTEFACT: &str = r#"{
  "v": 1,
  "kind": "onion",
  "chain_id": "stand",
  "private_key_hex": "1111111111111111111111111111111111111111111111111111111111111111",
  "self_id": "6f1a0000-0000-4000-8000-000000000000",
  "mesh_id": "mirage-vpn",
  "hops": [
    {"node_id": "node-0", "addr": "203.0.113.2:41443", "role": "entry", "server_pubkey_hex": "2222222222222222222222222222222222222222222222222222222222222222"},
    {"node_id": "node-1", "addr": "203.0.113.3:41443", "role": "exit", "server_pubkey_hex": "3333333333333333333333333333333333333333333333333333333333333333"}
  ],
  "operator_pubkey_hex": "4444444444444444444444444444444444444444444444444444444444444444",
  "operator_secret_hex": "5555555555555555555555555555555555555555555555555555555555555555",
  "identity_not_after_ms": 1726086400000,
  "mode": "full_auto",
  "label": "onion-1",
  "issued_at_ms": 1726000000000
}"#;

#[test]
fn the_fleets_artefact_stays_the_vpn_one() {
    let artefact = DeliveryArtefact::from_json(FLEET_ARTEFACT).expect("the fleet's artefact parses");
    artefact.validate().expect("and validates");
    match artefact {
        DeliveryArtefact::Vpn(vpn) => {
            assert_eq!(vpn.entrances.len(), 1);
            assert_eq!(vpn.mesh_id, "MESH2");
        }
        other => panic!("the fleet's artefact must stay the VPN one, got {other:?}"),
    }
}

#[test]
fn the_onion_artefact_is_read_as_the_onion_one() {
    let artefact = DeliveryArtefact::from_json(ONION_ARTEFACT).expect("the onion artefact parses");
    artefact.validate().expect("and validates");
    match artefact {
        DeliveryArtefact::Onion(onion) => {
            assert_eq!(onion.chain_id, "stand");
            assert_eq!(onion.hops.len(), 2);
            assert_eq!(onion.hops[0].role, mirage_core::config::HopRole::Entry);
            assert_eq!(onion.hops[1].role, mirage_core::config::HopRole::Exit);
        }
        other => panic!("the onion artefact must be the onion one, got {other:?}"),
    }
}

#[test]
fn the_onion_artefact_becomes_a_chain_the_client_can_walk() {
    let onion = OnionArtefact::from_json(ONION_ARTEFACT).expect("parse");
    let chain = onion.chain_config().expect("the chain config");
    assert_eq!(chain.chain_id, "stand");
    assert_eq!(chain.hops.len(), 2);
    assert_eq!(chain.hops[0].addr, "203.0.113.2:41443");
    assert_eq!(chain.hops[1].addr, "203.0.113.3:41443");
    assert_eq!(chain.hops[0].node_id.as_deref(), Some("node-0"));
    assert_eq!(chain.hops[1].node_id.as_deref(), Some("node-1"));
    chain.validate().expect("the onion builder accepts it");
}

#[test]
fn the_onion_artefact_becomes_the_chain_the_dialer_consumes() {
    let onion = OnionArtefact::from_json(ONION_ARTEFACT).expect("parse");
    let spec = onion.to_spec().expect("the dial spec");
    assert_eq!(spec.chain_id, "stand");
    assert_eq!(spec.hops.len(), 2);
    assert_eq!(spec.exit_node_id(), "node-1");
    assert_eq!(spec.mesh_id, "mirage-vpn");
}

#[test]
fn an_unknown_kind_is_refused_by_name() {
    let raw = r#"{"v": 1, "kind": "carrier-pigeon", "private_key_hex": "11"}"#;
    let err = DeliveryArtefact::from_json(raw).expect_err("an unknown kind is not an artefact");
    assert!(err.to_string().contains("carrier-pigeon"), "{err}");
}

#[test]
fn the_vpn_artefact_still_parses_on_its_own() {
    // The fleet's shape is untouched: the type the panel's own test writes
    // still parses through the same contract.
    let vpn = EntranceArtefact::from_json(FLEET_ARTEFACT).expect("the fleet's artefact parses");
    vpn.validate().expect("and validates");
    assert_eq!(vpn.entrances[0].addr, "203.0.113.9:443");
}
