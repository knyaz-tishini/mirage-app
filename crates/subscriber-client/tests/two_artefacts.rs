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
const ONION_ARTEFACT: &str = r#"{
  "v": 1,
  "kind": "onion",
  "chain_id": "stand",
  "private_key_hex": "1111111111111111111111111111111111111111111111111111111111111111",
  "mesh_id": "mirage-vpn",
  "hops": [
    {"addr": "203.0.113.2:41443", "role": "entry", "server_pubkey_hex": "2222222222222222222222222222222222222222222222222222222222222222"},
    {"addr": "203.0.113.3:41443", "role": "exit", "server_pubkey_hex": "3333333333333333333333333333333333333333333333333333333333333333"}
  ],
  "operator_pubkey_hex": "4444444444444444444444444444444444444444444444444444444444444444",
  "operator_secret_hex": "5555555555555555555555555555555555555555555555555555555555555555",
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
