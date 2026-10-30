//! The onion artefact — the second kind of delivery unit.
//!
//! # Two artefacts, one client
//!
//! The **VPN artefact** ([`crate::artefact::EntranceArtefact`]) names ONE
//! box: the edge. The customer dials it, the edge relays onward, and the
//! customer never learns what is behind it — that is the whole point of the
//! edge-as-entry arrangement.
//!
//! The **onion artefact** names a CHAIN. There is no edge to hide behind: the
//! customer IS the edge, the client walks the hops itself, and every hop
//! must therefore be named — its address, its key and the role it plays.
//!
//! # The two are told apart by one field
//!
//! The onion artefact carries `"kind": "onion"`. The VPN artefact carries no
//! such field — the fleet's own shape, which this crate does not change by a
//! byte. So the discriminant is **additive**: an artefact without `kind` is
//! the VPN one, exactly as before.
//!
//! # What the onion artefact must carry, and why
//!
//! - the **hops**, in the order the traffic walks them: the entry first, the
//!   exit last. The role is carried per hop, so a reader never has to guess;
//! - the chain's **trust anchor** (the public key every hop verifies the leg
//!   against) and the **operator secret** (the client signs the leg with it —
//!   in this mode the client is the edge, so it signs);
//! - the **mesh id**, folded into every handshake inside the chain;
//! - the client's own **static key**, the same one the VPN artefact carries.

use mirage_core::config::{ChainConfig, HopConfig, HopRole, TransportCamouflage};
use mirage_core::crypto::AeadSuite;
use mirage_core::mirage::wire::{ProtocolId, RelayMode, TcpProtocolId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::artefact::ArtefactMode;

/// The onion artefact's own version line.
///
/// Its own line, not the VPN artefact's: the two artefacts are separate
/// contracts, each with its own version, and neither is a revision of the
/// other.
pub const ONION_VERSION: u32 = 1;

/// The discriminant that tells the onion artefact from the VPN one.
pub const ONION_KIND: &str = "onion";

/// Expected decoded length, in bytes, of every 32-byte X25519 hex field.
const HEX32_BYTES: usize = 32;

/// The onion artefact — a chain the client walks itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnionArtefact {
    /// Artefact format version. Only [`ONION_VERSION`] is accepted.
    pub v: u32,
    /// Always [`ONION_KIND`] — the discriminant the client reads first.
    pub kind: String,
    /// The chain's id — the same in every box of the chain.
    pub chain_id: String,
    /// The subscriber's own persistent X25519 static secret, 64-char hex.
    ///
    /// The same key the VPN artefact carries: the customer's own identity,
    /// not the chain's.
    pub private_key_hex: String,
    /// The client's own identity in the mesh — the one the chain leg's
    /// credential names.
    ///
    /// A UUID in its canonical hyphenated form.
    pub self_id: String,
    /// Mesh identifier folded into every handshake inside the chain.
    pub mesh_id: String,
    /// The circuit's hops, in the order the traffic walks them: entry first,
    /// exit last.
    pub hops: Vec<OnionHop>,
    /// The chain's trust anchor — the public key every hop verifies the
    /// chain leg against.
    pub operator_pubkey_hex: String,
    /// The chain's operator secret — the client signs the chain leg with it.
    ///
    /// In the fleet the edge signs it; in this mode the client IS the edge,
    /// so the secret travels in the artefact — to the customer, who is the
    /// operator of this chain.
    pub operator_secret_hex: String,
    /// Unix-ms after which the identity must not be honoured — the
    /// subscription's paid-through time.
    ///
    /// The exit refuses a chain leg whose identity bound has passed.
    pub identity_not_after_ms: u64,
    /// Full-auto, or a named egress country — the same choice the VPN
    /// artefact carries.
    pub mode: ArtefactMode,
    /// A human tag for logs. Must never carry customer identity.
    pub label: String,
    pub issued_at_ms: u64,
}

/// One hop of the circuit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnionHop {
    /// This hop's identity in the mesh.
    ///
    /// Not decoration: the exit binds its credential check to this string,
    /// and the chain builder refuses a hop without it.
    pub node_id: String,
    /// `host:port` of this hop.
    pub addr: String,
    /// The role this hop plays: entry, middle or exit.
    pub role: HopRole,
    /// The hop's X25519 public key, 64-char hex — the key the chain leg
    /// verifies the hop by.
    pub server_pubkey_hex: String,
    /// The mask the hop wears, when it wears one. `None` — raw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camouflage: Option<TransportCamouflage>,
}

/// Every way the onion artefact is refused, by name.
#[derive(Debug, Error)]
pub enum OnionArtefactError {
    #[error("onion artefact: unsupported version {found} (this client understands {expected})")]
    UnsupportedVersion { found: u32, expected: u32 },

    #[error("onion artefact: kind is {found:?}, not {expected:?}")]
    WrongKind { found: String, expected: String },

    #[error("onion artefact: chain_id is empty")]
    EmptyChainId,

    #[error("onion artefact: mesh_id is empty")]
    EmptyMeshId,

    #[error("onion artefact: the trust anchor is empty")]
    EmptyAnchor,

    #[error("onion artefact: the operator secret is empty")]
    EmptyOperatorSecret,

    #[error("onion artefact: the identity bound is zero - the chain leg would expire at once")]
    EmptyIdentityBound,

    #[error("onion artefact: self_id {value:?} is not a UUID")]
    BadSelfId { value: String },

    #[error("onion artefact: hops[{index}].node_id is empty - the exit binds its credential check to it")]
    EmptyNodeId { index: usize },

    #[error("onion artefact: no hops — there is no chain to walk")]
    NoHops,

    #[error("onion artefact: hops[{index}].addr {addr:?} is not host:port")]
    BadHopAddr { index: usize, addr: String },

    #[error("onion artefact: hops[{index}] plays the role {found:?}, but at this place it must be {expected:?}")]
    BadHopRole {
        index: usize,
        found: HopRole,
        expected: HopRole,
    },

    #[error("onion artefact: field '{field}' is not valid hex: {reason}")]
    MalformedHex { field: String, reason: String },

    #[error(
        "onion artefact: field '{field}' must decode to {expected_bytes} bytes, got {got_bytes}"
    )]
    WrongHexLength { field: String, expected_bytes: usize, got_bytes: usize },

    #[error("onion artefact: invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

fn check_hex_field(field: &str, value: &str, expected_bytes: usize) -> Result<(), OnionArtefactError> {
    let bytes = hex::decode(value).map_err(|e| OnionArtefactError::MalformedHex {
        field: field.to_owned(),
        reason: e.to_string(),
    })?;
    if bytes.len() != expected_bytes {
        return Err(OnionArtefactError::WrongHexLength {
            field: field.to_owned(),
            expected_bytes,
            got_bytes: bytes.len(),
        });
    }
    Ok(())
}

/// `true` for a plain `host:port` pair — non-empty host, port parses as a
/// `u16`.
fn looks_like_host_port(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
        None => false,
    }
}

impl OnionArtefact {
    /// Parse an onion artefact from its JSON form. Does NOT validate — call
    /// [`OnionArtefact::validate`] before acting on the result.
    pub fn from_json(raw: &str) -> Result<Self, OnionArtefactError> {
        Ok(serde_json::from_str(raw)?)
    }

    /// Compact JSON form (no pretty-printing).
    pub fn to_json(&self) -> Result<String, OnionArtefactError> {
        Ok(serde_json::to_string(self)?)
    }

    /// Refuse a malformed artefact BY NAME. Only inspects `self` — never
    /// touches disk or the network.
    pub fn validate(&self) -> Result<(), OnionArtefactError> {
        if self.v != ONION_VERSION {
            return Err(OnionArtefactError::UnsupportedVersion {
                found: self.v,
                expected: ONION_VERSION,
            });
        }
        if self.kind != ONION_KIND {
            return Err(OnionArtefactError::WrongKind {
                found: self.kind.clone(),
                expected: ONION_KIND.to_owned(),
            });
        }
        if self.chain_id.trim().is_empty() {
            return Err(OnionArtefactError::EmptyChainId);
        }
        if self.mesh_id.trim().is_empty() {
            return Err(OnionArtefactError::EmptyMeshId);
        }
        if self.operator_pubkey_hex.trim().is_empty() {
            return Err(OnionArtefactError::EmptyAnchor);
        }
        if self.operator_secret_hex.trim().is_empty() {
            return Err(OnionArtefactError::EmptyOperatorSecret);
        }
        if self.identity_not_after_ms == 0 {
            return Err(OnionArtefactError::EmptyIdentityBound);
        }
        if uuid::Uuid::parse_str(self.self_id.trim()).is_err() {
            return Err(OnionArtefactError::BadSelfId { value: self.self_id.clone() });
        }
        check_hex_field("private_key_hex", &self.private_key_hex, HEX32_BYTES)?;
        check_hex_field("operator_pubkey_hex", &self.operator_pubkey_hex, HEX32_BYTES)?;
        if self.hops.is_empty() {
            return Err(OnionArtefactError::NoHops);
        }
        let count = self.hops.len();
        for (index, hop) in self.hops.iter().enumerate() {
            if hop.node_id.trim().is_empty() {
                return Err(OnionArtefactError::EmptyNodeId { index });
            }
            if !looks_like_host_port(&hop.addr) {
                return Err(OnionArtefactError::BadHopAddr {
                    index,
                    addr: hop.addr.clone(),
                });
            }
            check_hex_field(
                &format!("hops[{index}].server_pubkey_hex"),
                &hop.server_pubkey_hex,
                HEX32_BYTES,
            )?;
            // The role is not free: a hop's place in the chain decides what it
            // does, and a chain whose roles do not match its places is a
            // chain that would route the traffic wrong.
            let expected = role_for_position(index, count);
            if hop.role != expected {
                return Err(OnionArtefactError::BadHopRole {
                    index,
                    found: hop.role,
                    expected,
                });
            }
        }
        Ok(())
    }

    /// The subscriber's persistent static X25519 secret, decoded once.
    pub fn client_static_priv(&self) -> Result<[u8; 32], OnionArtefactError> {
        let bytes = hex::decode(&self.private_key_hex).map_err(|e| OnionArtefactError::MalformedHex {
            field: "private_key_hex".to_owned(),
            reason: e.to_string(),
        })?;
        let got_bytes = bytes.len();
        bytes.try_into().map_err(|_| OnionArtefactError::WrongHexLength {
            field: "private_key_hex".to_owned(),
            expected_bytes: HEX32_BYTES,
            got_bytes,
        })
    }

    /// The chain the client walks — the shape the onion dial path consumes.
    ///
    /// The hops in order, each with its address, its key, its role and its
    /// mask; the telescopic handshake, because it is the only onion handshake
    /// that exists; the replies onion-wrapped, because that is what a
    /// telescopic circuit does.
    pub fn chain_config(&self) -> Result<ChainConfig, OnionArtefactError> {
        self.validate()?;
        let hops = self
            .hops
            .iter()
            .map(|hop| HopConfig {
                addr: hop.addr.clone(),
                public_key: Some(hop.server_pubkey_hex.clone()),
                node_id: Some(hop.node_id.clone()),
                sni: None,
                role: hop.role,
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

    /// The chain the onion plane walks — the delivery artefact in the shape
    /// the dial path consumes.
    ///
    /// The two contracts are separate on purpose: the artefact is what the
    /// operator hands over, the spec is what the dialer eats. This is the one
    /// place they meet.
    pub fn to_spec(&self) -> Result<mirage_onion_client::OnionChainSpec, OnionArtefactError> {
        self.validate()?;
        let self_id = uuid::Uuid::parse_str(self.self_id.trim()).map_err(|_| {
            OnionArtefactError::BadSelfId { value: self.self_id.clone() }
        })?;
        Ok(mirage_onion_client::OnionChainSpec {
            chain_id: self.chain_id.clone(),
            mesh_id: self.mesh_id.clone(),
            self_id,
            operator_pubkey_hex: self.operator_pubkey_hex.clone(),
            operator_secret_hex: self.operator_secret_hex.clone(),
            identity_not_after_ms: self.identity_not_after_ms,
            client_static_priv: self.client_static_priv()?,
            hops: self
                .hops
                .iter()
                .map(|hop| mirage_onion_client::OnionHopSpec {
                    node_id: hop.node_id.clone(),
                    addr: hop.addr.clone(),
                    server_pubkey_hex: hop.server_pubkey_hex.clone(),
                    camouflage: hop.camouflage.clone(),
                })
                .collect(),
        })
    }
}

/// The role a hop plays at `position` among `count` hops.
///
/// The same rule the chain's own boxes are given (`MIRAGE_CHAIN_POSITION`):
/// the first hop takes the chain from the edge, the last one ends it, and
/// everything between relays onward. A circuit of ONE hop is the exit.
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

    fn hop(index: usize, count: usize) -> OnionHop {
        OnionHop {
            node_id: format!("node-{index}"),
            addr: format!("203.0.113.{}:41443", index + 1),
            role: role_for_position(index, count),
            server_pubkey_hex: format!("{:02x}", index + 1).repeat(HEX32_BYTES),
            camouflage: Some(TransportCamouflage::WireGuard {
                magic_byte: 1,
                junk_count: 4,
                junk_min: 10,
                junk_max: 20,
                bind_addr: "0.0.0.0:41443".parse().expect("addr"),
            }),
        }
    }

    fn sample(count: usize) -> OnionArtefact {
        OnionArtefact {
            v: ONION_VERSION,
            kind: ONION_KIND.to_owned(),
            chain_id: "stand".to_owned(),
            private_key_hex: "11".repeat(HEX32_BYTES),
            self_id: "6f1a0000-0000-4000-8000-000000000000".to_owned(),
            mesh_id: "mirage-vpn".to_owned(),
            hops: (0..count).map(|i| hop(i, count)).collect(),
            operator_pubkey_hex: "22".repeat(HEX32_BYTES),
            operator_secret_hex: "33".repeat(HEX32_BYTES),
            identity_not_after_ms: 1_726_086_400_000,
            mode: ArtefactMode::FullAuto,
            label: "onion-1".to_owned(),
            issued_at_ms: 1_726_000_000_000,
        }
    }

    #[test]
    fn a_two_hop_chain_validates() {
        assert!(sample(2).validate().is_ok());
    }

    #[test]
    fn a_single_hop_is_the_exit() {
        let artefact = sample(1);
        artefact.validate().expect("one hop is a valid chain");
        assert_eq!(artefact.hops[0].role, HopRole::Exit);
    }

    #[test]
    fn the_roles_follow_the_places() {
        let artefact = sample(3);
        artefact.validate().expect("three hops");
        assert_eq!(artefact.hops[0].role, HopRole::Entry);
        assert_eq!(artefact.hops[1].role, HopRole::Middle);
        assert_eq!(artefact.hops[2].role, HopRole::Exit);
    }

    #[test]
    fn a_role_that_does_not_match_the_place_is_refused() {
        let mut artefact = sample(3);
        artefact.hops[0].role = HopRole::Exit;
        assert!(matches!(
            artefact.validate(),
            Err(OnionArtefactError::BadHopRole { index: 0, .. })
        ));
    }

    #[test]
    fn an_artefact_without_hops_is_refused() {
        let mut artefact = sample(2);
        artefact.hops.clear();
        assert!(matches!(artefact.validate(), Err(OnionArtefactError::NoHops)));
    }

    #[test]
    fn the_kind_is_checked() {
        let mut artefact = sample(2);
        artefact.kind = "vpn".to_owned();
        assert!(matches!(
            artefact.validate(),
            Err(OnionArtefactError::WrongKind { .. })
        ));
    }

    #[test]
    fn the_version_is_checked() {
        let mut artefact = sample(2);
        artefact.v = 7;
        assert!(matches!(
            artefact.validate(),
            Err(OnionArtefactError::UnsupportedVersion { found: 7, expected: 1 })
        ));
    }

    #[test]
    fn a_bad_hop_address_is_refused() {
        let mut artefact = sample(2);
        artefact.hops[1].addr = "not-an-addr".to_owned();
        assert!(matches!(
            artefact.validate(),
            Err(OnionArtefactError::BadHopAddr { index: 1, .. })
        ));
    }

    #[test]
    fn a_bad_key_is_refused_by_name() {
        let mut artefact = sample(2);
        artefact.hops[0].server_pubkey_hex = "not-hex".to_owned();
        match artefact.validate() {
            Err(OnionArtefactError::MalformedHex { field, .. }) => {
                assert_eq!(field, "hops[0].server_pubkey_hex")
            }
            other => panic!("expected MalformedHex, got {other:?}"),
        }
    }

    #[test]
    fn it_round_trips_through_json() {
        let artefact = sample(2);
        let json = artefact.to_json().expect("json");
        let back = OnionArtefact::from_json(&json).expect("parse");
        assert_eq!(artefact.chain_id, back.chain_id);
        assert_eq!(artefact.hops.len(), back.hops.len());
        back.validate().expect("and it validates");
    }

    #[test]
    fn the_chain_config_carries_every_hop() {
        let artefact = sample(3);
        let chain = artefact.chain_config().expect("the chain config");
        assert_eq!(chain.chain_id, "stand");
        assert_eq!(chain.hops.len(), 3);
        assert_eq!(chain.hops[0].addr, "203.0.113.1:41443");
        assert_eq!(chain.hops[0].role, HopRole::Entry);
        assert_eq!(chain.hops[2].role, HopRole::Exit);
        assert_eq!(
            chain.hops[0].public_key.as_deref(),
            Some(artefact.hops[0].server_pubkey_hex.as_str()),
            "the hop's key must reach the chain config"
        );
        assert_eq!(
            chain.hops[0].node_id.as_deref(),
            Some("node-0"),
            "the hop's identity must reach the chain config"
        );
    }

    #[test]
    fn the_chain_config_passes_the_builders_own_validation() {
        let chain = sample(2).chain_config().expect("the chain config");
        assert!(
            chain.validate().is_ok(),
            "the onion builder refuses a chain without node ids"
        );
    }

    #[test]
    fn a_hop_without_a_node_id_is_refused() {
        let mut artefact = sample(2);
        artefact.hops[1].node_id = String::new();
        assert!(matches!(
            artefact.validate(),
            Err(OnionArtefactError::EmptyNodeId { index: 1 })
        ));
    }

    #[test]
    fn a_self_id_that_is_not_a_uuid_is_refused() {
        let mut artefact = sample(2);
        artefact.self_id = "not-a-uuid".to_owned();
        assert!(matches!(
            artefact.validate(),
            Err(OnionArtefactError::BadSelfId { .. })
        ));
    }

    #[test]
    fn an_identity_bound_of_zero_is_refused() {
        let mut artefact = sample(2);
        artefact.identity_not_after_ms = 0;
        assert!(matches!(
            artefact.validate(),
            Err(OnionArtefactError::EmptyIdentityBound)
        ));
    }

    #[test]
    fn the_spec_carries_everything_the_dial_needs() {
        let artefact = sample(2);
        let spec = artefact.to_spec().expect("the spec");
        assert_eq!(spec.chain_id, "stand");
        assert_eq!(spec.mesh_id, "mirage-vpn");
        assert_eq!(spec.hops.len(), 2);
        assert_eq!(spec.hops[0].node_id, "node-0");
        assert_eq!(spec.exit_node_id(), "node-1");
        assert_eq!(spec.identity_not_after_ms, artefact.identity_not_after_ms);
        assert_eq!(spec.client_static_priv, [0x11u8; 32]);
    }
}
