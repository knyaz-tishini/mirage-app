//! `mirage-onion-client` - the onion plane: the client walks the chain itself.
//!
//! # What this crate is
//!
//! A chain of hops the client dials by itself, with no edge in front of it:
//! the client is the edge. It builds the chain's telescopic handshake, signs
//! the chain leg with the chain's operator key, and hands back a tunnel that
//! already carries the exit's mux session.
//!
//! # What this crate is not
//!
//! It is not the thin customer connector. That one (`mirage-subscriber-client`)
//! holds no mesh credential at all and talks only to its own edge box; this
//! one holds the operator secret and walks the chain. Keeping them apart is
//! the point: the thin connector stays thin, and the operator's key never
//! becomes reachable from it.
//!
//! # The shape of the dial
//!
//! 1. The chain spec (see [`spec`]) is validated and turned into the chain
//!    config the onion builder consumes.
//! 2. The chain leg's credential is minted - the operator signs the identity
//!    excerpt, the client signs the dial (see [`credential`]).
//! 3. The chain is dialled through `establish_mux_tunnel`, which walks the
//!    telescopic handshake hop by hop and ends with the exit's mux session.
//!
//! Two hops - an entry and an exit - are enough for the whole chain to work;
//! more hops are relayed by the ones in between.

pub mod credential;
pub mod spec;

pub use credential::{mint_chain_leg_credential, CredentialError};
pub use spec::{OnionChainSpec, OnionHopSpec, SpecError};

use std::time::{SystemTime, UNIX_EPOCH};

use mirage_control_plane::establish_mux_tunnel;
use mirage_core::config::{AuthMode, MirageEndpointConfig, TcpProtocolId, TransportCamouflage, TransportKind};
use mirage_core::error::MirageError;
use mirage_core::mirage::mesh::PeerTable;
use mirage_protocol::driver::ClientCredential;
use mirage_protocol::endpoint_protocol::EndpointProtocol;
use mirage_protocol::negotiation::ProtocolChain;
use mirage_protocol::traits::{TcpProtocol, Tunnel};
use thiserror::Error;

/// The wire version the chain leg speaks.
///
/// V9 is the version the node accepts - V8 has no credential slot, and a leg
/// without a credential is a leg the hop refuses.
pub const LEG_WIRE_VERSION: u8 = 9;

/// Every way walking an onion chain fails.
#[derive(Debug, Error)]
pub enum OnionDialError {
    #[error(transparent)]
    Spec(#[from] SpecError),

    #[error(transparent)]
    Credential(#[from] CredentialError),

    #[error(transparent)]
    Mirage(#[from] MirageError),
}

/// Unix-ms, the clock every credential timestamp is measured against.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Build the protocol chain the leg is spoken over.
///
/// The camouflage is `None` here on purpose: the leg's OUTER disguise is the
/// entry hop's own published face, and it is applied by the onion builder,
/// not by the handshake.
pub fn build_protocol_chain(
    mesh_id: &[u8],
    credential: Option<ClientCredential>,
) -> ProtocolChain {
    let config = MirageEndpointConfig {
        transport: TransportKind::Tcp,
        auth: AuthMode::Blake3,
        camouflage: TransportCamouflage::None,
        byte_cipher: None,
        byte_cipher_mode: None,
    };
    let proto: Box<dyn TcpProtocol> = Box::new(
        EndpointProtocol::new(config, TcpProtocolId::Mirage, mesh_id.to_vec())
            .with_wire_version(LEG_WIRE_VERSION)
            .with_credential(credential),
    );
    ProtocolChain::new(vec![proto])
}

/// Walk the chain and end with the exit's mux session.
///
/// The returned tunnel is already an upgraded mux session: the exit saw the
/// mux magic and switched to its mux server, so the caller can open streams
/// on it directly. Each stream's first bytes are the exit's own target frame
/// (`[2B len][target]`), not the edge session's `StreamOpen` header.
pub async fn dial(spec: &OnionChainSpec) -> Result<Box<dyn Tunnel>, OnionDialError> {
    let chain = spec.chain_config()?;
    let credential = mint_chain_leg_credential(spec, now_ms())?;
    let protocols = build_protocol_chain(spec.mesh_id_bytes(), Some(credential.clone()));
    // The chain names every hop's key and identity by itself - there is no
    // peer table to consult, and the client holds none.
    let peer_table = PeerTable::empty();
    let tunnel = establish_mux_tunnel(
        &protocols,
        &chain,
        &peer_table,
        spec.mesh_id_bytes(),
        Some(&credential),
        None,
    )
    .await?;
    Ok(tunnel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::OnionHopSpec;
    use ed25519_dalek::SigningKey;
    use uuid::Uuid;

    fn spec(count: usize) -> OnionChainSpec {
        let key = SigningKey::from_bytes(&[0x42u8; 32]);
        OnionChainSpec {
            chain_id: "stand".to_owned(),
            mesh_id: "mirage-vpn".to_owned(),
            self_id: Uuid::new_v4(),
            operator_pubkey_hex: hex::encode(key.verifying_key().to_bytes()),
            operator_secret_hex: hex::encode(key.to_bytes()),
            identity_not_after_ms: 1_726_000_000_000,
            client_static_priv: [0x11; 32],
            hops: (0..count)
                .map(|i| OnionHopSpec {
                    node_id: format!("node-{i}"),
                    addr: format!("203.0.113.{}:41443", i + 1),
                    server_pubkey_hex: format!("{:02x}", i + 1).repeat(32),
                    camouflage: None,
                })
                .collect(),
        }
    }

    #[test]
    fn the_protocol_chain_carries_the_credential() {
        let credential = mint_chain_leg_credential(&spec(2), now_ms()).expect("credential");
        // A chain with no protocols panics on construction - building one at
        // all is the assertion here.
        let _chain = build_protocol_chain(b"mirage-vpn", Some(credential));
    }

    #[test]
    fn a_malformed_spec_is_refused_before_any_dial() {
        let mut spec = spec(2);
        spec.hops[0].addr = "no-port".to_owned();
        assert!(matches!(spec.chain_config(), Err(SpecError::BadHopAddr { index: 0, .. })));
    }
}
