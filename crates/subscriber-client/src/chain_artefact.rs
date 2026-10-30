//! Building the onion artefact from a chain the client built itself.
//!
//! # The seam this closes
//!
//! The VPN artefact is built from a box's publication ([`crate::oss_artefact`]):
//! one box, its faces, the client's own key.
//!
//! The onion artefact is built from a **chain** — the boxes the client
//! installed, in the order the traffic walks them. The client is the edge of
//! that chain, so the artefact carries the hop list AND the key material the
//! edge signs the chain leg with.
//!
//! Both artefacts are ordinary artefacts of their own kind: the client reads
//! either, and neither is a revision of the other.

use mirage_client_chain::chain::Chain;
use mirage_core::config::TransportCamouflage;
use mirage_core::crypto::KeyPair;
use thiserror::Error;
use uuid::Uuid;

use crate::artefact::ArtefactMode;
use crate::onion_artefact::{role_for_position, OnionArtefact, OnionHop, ONION_KIND, ONION_VERSION};

/// Every way building an onion artefact from a chain can fail, by name.
#[derive(Debug, Error)]
pub enum ChainArtefactError {
    /// The chain has no hops: there is nothing to walk.
    #[error("the chain has no hops — there is no circuit to walk")]
    NoHops,

    /// The built artefact does not pass the wire contract's own validation.
    #[error("the onion artefact does not validate: {0}")]
    Invalid(#[from] crate::onion_artefact::OnionArtefactError),
}

/// Build the onion artefact from a chain.
///
/// The chain's hops are the circuit: the entry first, the exit last, each
/// with the role its place gives it. The chain's own key material travels
/// with it — the anchor every hop verifies the leg against, and the secret
/// the client signs the leg with, because in this mode the client is the
/// edge.
pub fn build_onion_artefact(
    chain: &Chain,
    client_keypair: &KeyPair,
    label: &str,
    issued_at_ms: u64,
) -> Result<OnionArtefact, ChainArtefactError> {
    let hops = chain.hops();
    if hops.is_empty() {
        return Err(ChainArtefactError::NoHops);
    }
    let count = hops.len();
    let mut built = Vec::with_capacity(count);
    for (index, node) in hops.iter().enumerate() {
        built.push(OnionHop {
            // The box's own name in the chain is its identity there: the exit
            // binds the chain leg's credential to it, so the name the chain
            // was built with is the name the hop answers to.
            node_id: node.host.clone(),
            addr: node.face.addr.clone(),
            role: role_for_position(index, count),
            server_pubkey_hex: node.face.pubkey_hex.clone(),
            // The mask is the JSON the box was given; a hop that wears none
            // carries a null there, and that is not a failure — it is raw.
            camouflage: serde_json::from_str::<TransportCamouflage>(&node.face.camouflage).ok(),
        });
    }

    let artefact = OnionArtefact {
        v: ONION_VERSION,
        kind: ONION_KIND.to_owned(),
        chain_id: chain.chain_id.clone(),
        private_key_hex: hex::encode(client_keypair.secret.to_bytes()),
        self_id: client_identity(client_keypair).to_string(),
        mesh_id: chain.keys.mesh_id.clone(),
        hops: built,
        operator_pubkey_hex: chain.keys.operator_pubkey_hex.clone(),
        operator_secret_hex: chain.keys.operator_secret_hex.clone(),
        identity_not_after_ms: issued_at_ms.saturating_add(DEFAULT_SUBSCRIPTION_MS),
        mode: ArtefactMode::FullAuto,
        label: label.to_owned(),
        issued_at_ms,
    };
    artefact.validate()?;
    Ok(artefact)
}

/// How long the identity the artefact carries stays valid, when the issuer
/// says nothing about it.
///
/// Thirty days — the subscription's shape. The exit refuses the chain leg
/// once the bound has passed, so a stale artefact stops working on its own.
pub const DEFAULT_SUBSCRIPTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// The client's own identity in the mesh, derived from its own key.
///
/// Derived, not random: rebuilding the same chain from the same client key
/// must produce the same identity, or the customer's chain leg would be
/// refused by an exit that has already seen the previous one.
fn client_identity(keypair: &KeyPair) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&keypair.public.to_bytes()[..16]);
    // RFC 4122 shape: version 4, variant 1.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// The clock the artefact is stamped with.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_client_chain::chain::{ChainBox, ChainFace, ChainKeys};

    fn chain(hops: usize) -> Chain {
        let mut boxes = Vec::new();
        boxes.push(ChainBox {
            host: "edge.example".to_owned(),
            user: "root".to_owned(),
            face: ChainFace {
                addr: "203.0.113.1:41443".to_owned(),
                pubkey_hex: "aa".repeat(32),
                camouflage: "null".to_owned(),
            },
        });
        for index in 0..hops {
            boxes.push(ChainBox {
                host: format!("hop{index}.example"),
                user: "root".to_owned(),
                face: ChainFace {
                    addr: format!("203.0.113.{}:41443", index + 2),
                    pubkey_hex: format!("{:02x}", index + 1).repeat(32),
                    camouflage: r#"{"type":"wire_guard","magic_byte":1,"junk_count":4,"junk_min":10,"junk_max":20,"bind_addr":"0.0.0.0:41443"}"#.to_owned(),
                },
            });
        }
        Chain {
            chain_id: "stand".to_owned(),
            boxes,
            keys: ChainKeys {
                operator_pubkey_hex: "bb".repeat(32),
                operator_secret_hex: "cc".repeat(32),
                mesh_id: "mirage-vpn".to_owned(),
            },
        }
    }

    #[test]
    fn the_circuit_is_the_chains_hops() {
        let keypair = KeyPair::generate();
        let artefact = build_onion_artefact(&chain(2), &keypair, "onion-1", 0).expect("build");
        artefact.validate().expect("and it validates");
        assert_eq!(artefact.hops.len(), 2, "the edge is not part of the circuit");
        assert_eq!(artefact.hops[0].addr, "203.0.113.2:41443");
        assert_eq!(artefact.hops[0].role, mirage_core::config::HopRole::Entry);
        assert_eq!(artefact.hops[1].role, mirage_core::config::HopRole::Exit);
    }

    #[test]
    fn the_chain_key_material_travels_with_the_artefact() {
        let keypair = KeyPair::generate();
        let artefact = build_onion_artefact(&chain(1), &keypair, "onion-1", 0).expect("build");
        assert_eq!(artefact.operator_pubkey_hex, "bb".repeat(32));
        assert_eq!(artefact.operator_secret_hex, "cc".repeat(32));
        assert_eq!(artefact.mesh_id, "mirage-vpn");
        assert_eq!(artefact.chain_id, "stand");
    }

    #[test]
    fn the_clients_own_key_is_the_one_the_artefact_carries() {
        let keypair = KeyPair::generate();
        let artefact = build_onion_artefact(&chain(1), &keypair, "onion-1", 0).expect("build");
        assert_eq!(
            artefact.private_key_hex,
            hex::encode(keypair.secret.to_bytes())
        );
    }

    #[test]
    fn a_chain_without_hops_has_no_circuit() {
        let keypair = KeyPair::generate();
        let err = build_onion_artefact(&chain(0), &keypair, "onion-1", 0)
            .expect_err("a chain of one box has nothing to walk");
        assert!(matches!(err, ChainArtefactError::NoHops));
    }
}
