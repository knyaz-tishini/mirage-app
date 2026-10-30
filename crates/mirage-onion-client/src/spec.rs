//! The chain the onion plane walks — the dial contract.
//!
//! The delivery artefact (the customer's file) and this spec are two
//! different contracts on purpose: the artefact is what the operator hands
//! over, this is what the dial path consumes. The conversion happens once, in
//! the crate that owns both.

use mirage_core::config::{ChainConfig, HopConfig, HopRole, TransportCamouflage};
use mirage_core::crypto::AeadSuite;
use mirage_core::mirage::wire::{ProtocolId, RelayMode, TcpProtocolId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// One hop of the chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnionHopSpec {
    /// This hop's identity in the mesh.
    ///
    /// Not decoration: the exit binds its credential check to this string, and
    /// the chain builder refuses a hop without it.
    pub node_id: String,
    /// `host:port` of this hop.
    pub addr: String,
    /// The hop's X25519 public key, 64-char hex — the key the chain leg
    /// verifies the hop by.
    pub server_pubkey_hex: String,
    /// The mask the hop wears, when it wears one. `None` — raw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camouflage: Option<TransportCamouflage>,
}

/// The chain, in the shape the onion dial consumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnionChainSpec {
    /// The chain's id — the same in every box of the chain.
    pub chain_id: String,
    /// Mesh identifier folded into every handshake inside the chain.
    ///
    /// A string, not bytes: the exit compares it as UTF-8 bytes, and the
    /// credential carries it as a string.
    pub mesh_id: String,
    /// The client's own identity in the mesh — the one the credential names.
    pub self_id: Uuid,
    /// The chain's trust anchor: the operator's public key.
    pub operator_pubkey_hex: String,
    /// The operator's secret: the client signs the chain leg with it.
    ///
    /// In this mode the client IS the edge, so it signs.
    pub operator_secret_hex: String,
    /// Unix-ms after which the identity must not be honoured — the
    /// subscription's paid-through time.
    pub identity_not_after_ms: u64,
    /// The client's own persistent X25519 static secret, 32 bytes.
    pub client_static_priv: [u8; 32],
    /// The hops, in the order the traffic walks them: entry first, exit last.
    pub hops: Vec<OnionHopSpec>,
}

/// Every way a chain spec is refused, by name.
#[derive(Debug, Error)]
pub enum SpecError {
    #[error("onion spec: chain_id is empty")]
    EmptyChainId,

    #[error("onion spec: mesh_id is empty")]
    EmptyMeshId,

    #[error("onion spec: no hops - there is no chain to walk")]
    NoHops,

    #[error("onion spec: the trust anchor is empty")]
    EmptyAnchor,

    #[error("onion spec: the operator secret is empty")]
    EmptyOperatorSecret,

    #[error("onion spec: the identity bound is zero - the credential would expire at once")]
    EmptyIdentityBound,

    #[error("onion spec: hops[{index}].node_id is empty - the exit binds its credential check to it")]
    EmptyNodeId { index: usize },

    #[error("onion spec: hops[{index}].addr {addr:?} is not host:port")]
    BadHopAddr { index: usize, addr: String },

    #[error("onion spec: field '{field}' is not valid hex: {reason}")]
    MalformedHex { field: String, reason: String },

    #[error("onion spec: field '{field}' must decode to {expected_bytes} bytes, got {got_bytes}")]
    WrongHexLength { field: String, expected_bytes: usize, got_bytes: usize },
}

/// Expected decoded length, in bytes, of every 32-byte hex field.
const HEX32_BYTES: usize = 32;

/// `true` for a plain `host:port` pair - non-empty host, port parses as a
/// `u16`.
fn looks_like_host_port(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
        None => false,
    }
}

fn check_hex_field(field: &str, value: &str, expected_bytes: usize) -> Result<(), SpecError> {
    let bytes = hex::decode(value).map_err(|e| SpecError::MalformedHex {
        field: field.to_owned(),
        reason: e.to_string(),
    })?;
    if bytes.len() != expected_bytes {
        return Err(SpecError::WrongHexLength {
            field: field.to_owned(),
            expected_bytes,
            got_bytes: bytes.len(),
        });
    }
    Ok(())
}

impl OnionChainSpec {
    /// Refuse a malformed chain BY NAME. Never touches the network.
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.chain_id.trim().is_empty() {
            return Err(SpecError::EmptyChainId);
        }
        if self.mesh_id.trim().is_empty() {
            return Err(SpecError::EmptyMeshId);
        }
        if self.operator_pubkey_hex.trim().is_empty() {
            return Err(SpecError::EmptyAnchor);
        }
        if self.operator_secret_hex.trim().is_empty() {
            return Err(SpecError::EmptyOperatorSecret);
        }
        if self.identity_not_after_ms == 0 {
            return Err(SpecError::EmptyIdentityBound);
        }
        check_hex_field("operator_pubkey_hex", &self.operator_pubkey_hex, HEX32_BYTES)?;
        check_hex_field("operator_secret_hex", &self.operator_secret_hex, HEX32_BYTES)?;
        if self.hops.is_empty() {
            return Err(SpecError::NoHops);
        }
        for (index, hop) in self.hops.iter().enumerate() {
            if hop.node_id.trim().is_empty() {
                return Err(SpecError::EmptyNodeId { index });
            }
            if !looks_like_host_port(&hop.addr) {
                return Err(SpecError::BadHopAddr {
                    index,
                    addr: hop.addr.clone(),
                });
            }
            check_hex_field(
                &format!("hops[{index}].server_pubkey_hex"),
                &hop.server_pubkey_hex,
                HEX32_BYTES,
            )?;
        }
        Ok(())
    }

    /// The mesh id as the handshake carries it.
    pub fn mesh_id_bytes(&self) -> &[u8] {
        self.mesh_id.as_bytes()
    }

    /// The exit hop's identity - the credential binds the dial to it, so a
    /// token lifted to another box does not verify there.
    pub fn exit_node_id(&self) -> &str {
        self.hops
            .last()
            .map(|h| h.node_id.as_str())
            .unwrap_or_default()
    }

    /// The chain the dial path hands to the onion builder.
    ///
    /// The telescopic handshake, because it is the only onion handshake that
    /// exists; onion-wrapped replies, because that is what a telescopic
    /// circuit does; every hop peeling, because that is what a chain of hops
    /// does.
    pub fn chain_config(&self) -> Result<ChainConfig, SpecError> {
        self.validate()?;
        let count = self.hops.len();
        let hops = self
            .hops
            .iter()
            .enumerate()
            .map(|(index, hop)| HopConfig {
                addr: hop.addr.clone(),
                public_key: Some(hop.server_pubkey_hex.clone()),
                node_id: Some(hop.node_id.clone()),
                sni: None,
                role: role_for_position(index, count),
                display_name: None,
                camouflage: hop.camouflage.clone(),
                byte_cipher: None,
                camo_server_pubkey: None,
                selector_token_hex: None,
                relay_mode: RelayMode::OnionPeel,
            })
            .collect();
        Ok(ChainConfig {
            chain_id: self.chain_id.clone(),
            hops,
            handshake: mirage_core::config::HandshakeStrategy::Telescopic,
            reply: mirage_core::config::ReplyMode::Onion,
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
        })
    }
}

/// The role a hop plays at `position` among `count` hops.
///
/// The same rule the chain's own boxes are given: the first hop takes the
/// chain from the edge, the last one ends it, everything between relays
/// onward. A circuit of ONE hop is the exit.
pub fn role_for_position(position: usize, count: usize) -> HopRole {
    if count <= 1 {
        HopRole::Exit
    } else if position == 0 {
        HopRole::Entry
    } else if position == count - 1 {
        HopRole::Exit
    } else {
        HopRole::Middle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(index: usize, count: usize) -> OnionHopSpec {
        OnionHopSpec {
            node_id: format!("node-{index}"),
            addr: format!("203.0.113.{}:41443", index + 1),
            server_pubkey_hex: format!("{:02x}", index + 1).repeat(HEX32_BYTES),
            camouflage: None,
        }
    }

    fn sample(count: usize) -> OnionChainSpec {
        OnionChainSpec {
            chain_id: "stand".to_owned(),
            mesh_id: "mirage-vpn".to_owned(),
            self_id: Uuid::new_v4(),
            operator_pubkey_hex: "22".repeat(HEX32_BYTES),
            operator_secret_hex: "33".repeat(HEX32_BYTES),
            identity_not_after_ms: 1_726_000_000_000,
            client_static_priv: [0x11; 32],
            hops: (0..count).map(|i| hop(i, count)).collect(),
        }
    }

    #[test]
    fn a_two_hop_chain_validates() {
        assert!(sample(2).validate().is_ok());
    }

    #[test]
    fn the_chain_config_carries_every_node_id() {
        let chain = sample(2).chain_config().expect("chain config");
        assert_eq!(chain.hops.len(), 2);
        assert_eq!(chain.hops[0].node_id.as_deref(), Some("node-0"));
        assert_eq!(chain.hops[1].node_id.as_deref(), Some("node-1"));
        assert!(chain.validate().is_ok(), "the builder's own validation must pass");
    }

    #[test]
    fn the_exit_is_the_last_hop() {
        let spec = sample(3);
        assert_eq!(spec.exit_node_id(), "node-2");
    }

    #[test]
    fn a_hop_without_a_node_id_is_refused() {
        let mut spec = sample(2);
        spec.hops[1].node_id = String::new();
        assert!(matches!(spec.validate(), Err(SpecError::EmptyNodeId { index: 1 })));
    }

    #[test]
    fn a_chain_without_hops_is_refused() {
        let mut spec = sample(2);
        spec.hops.clear();
        assert!(matches!(spec.validate(), Err(SpecError::NoHops)));
    }

    #[test]
    fn the_roles_follow_the_places() {
        let chain = sample(3).chain_config().expect("chain config");
        assert_eq!(chain.hops[0].role, HopRole::Entry);
        assert_eq!(chain.hops[1].role, HopRole::Middle);
        assert_eq!(chain.hops[2].role, HopRole::Exit);
    }
}
