//! `EntranceArtefact` — the delivery unit this crate consumes.
//!
//! Shaped after `docs/mirage/plans/delivery-artefact-and-issuance-2026-09-17.md`
//! §"The artefact", with two fields that plan's own draft does not carry:
//! `mesh_id` and, per entrance, `server_pubkey_hex`. That is not a departure
//! for its own sake — `docs/mirage/plans/thin-customer-connector-2026-09-17.md`
//! §2.1 names their absence as the one blocking gap in this whole track:
//! `handshake_client_stream_v9` requires `server_static_pub: &[u8; 32]` and
//! folds `mesh_id: &[u8]` into the client proof
//! (`mirage2web/crates/mirage-protocol/src/driver.rs:1528,1531`), and neither
//! exists anywhere on the artefact as that companion plan drafted it. Since
//! no crate in this workspace has landed `EntranceArtefact` yet (grepped
//! empty at the time of writing), this type is free to close that gap
//! locally rather than parse a shape that cannot be dialed. When the panel /
//! opbox issuance side (`mirage2panel`, `mirage2operator` — out of this
//! crate's scope) lands its own wire encoding, it must emit these two fields
//! or a translator must sit between the two shapes; that is follow-up work,
//! reported, not solved here.
//!
//! Three-step discipline, mirroring `l0-sentinel::access_packet` (the K2
//! artefact this replaces for the commercial track): parse
//! ([`EntranceArtefact::from_json`]) never validates; [`EntranceArtefact::validate`]
//! refuses a malformed artefact BY NAME, touching nothing; callers assemble
//! (`crate::assembly::SubscriberClient::build`) only after a successful
//! validate.
//!
//! **What this type structurally cannot carry**, matching the companion
//! plan's own enforcement style: no node id, no peer-table row, no
//! `credential_params`/capability-token field, no hop list, no chain
//! composition. `entrances: Vec<ArtefactEntrance>` is flat — one entry per
//! published face, never grouped by box — and each entry describes exactly
//! one dialable face. There is nowhere on this type to put a second hop.

use mirage_core::config::TransportCamouflage;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The only artefact version this crate understands.
pub const CURRENT_VERSION: u32 = 1;

/// Expected decoded length, in bytes, of every 32-byte X25519 hex field.
const HEX32_BYTES: usize = 32;

/// The customer's delivery artefact — see the module doc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntranceArtefact {
    /// Artefact format version. Only [`CURRENT_VERSION`] is accepted.
    pub v: u32,
    /// The subscriber's own persistent X25519 static secret, 64-char hex.
    /// Presented identically on every dial — this is what a box's
    /// `SubscriberKeyStore::contains` checks the client against. Never
    /// regenerated per-dial the way `EndpointProtocol::connect` regenerates
    /// an ephemeral key (`mirage2web/crates/mirage-protocol/src/
    /// endpoint_protocol.rs:99-100`) — see [`crate::dial::StaticKeyProtocol`].
    pub private_key_hex: String,
    /// Mesh identifier folded into every V9 handshake's client proof
    /// (`handshake_client_stream_v9`'s `mesh_id: &[u8]` parameter). Required
    /// — without it dial code cannot be written at all (see module doc).
    pub mesh_id: String,
    /// Every currently-published entrance face this subscriber may dial.
    /// Plural because several boxes are several independent entrances
    /// (owner's ruling, `docs/mirage/plans/ruling-2026-09-17-edge-boxes-
    /// trust-and-tiers.md` §4) and one box may itself publish more than one
    /// face. Never grouped by box on the wire.
    pub entrances: Vec<ArtefactEntrance>,
    /// Full-auto, or a named egress country. `Country` is accepted and
    /// carried even though nothing on the dial path honours it yet (the
    /// box-to-monitor plumbing named in the companion plan is unstarted) —
    /// a client applying this artefact must log the no-op, never silently
    /// ignore it. See `crate::assembly::SubscriberClient::build`.
    pub mode: ArtefactMode,
    /// A human tag for logs. Must never carry customer identity — not
    /// validated as one, must not be read as one anywhere.
    pub label: String,
    pub issued_at_ms: u64,
}

/// One dialable face — see [`EntranceArtefact::entrances`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtefactEntrance {
    /// `host:port` of this published face.
    pub addr: String,
    /// The box's static X25519 public key for THIS face, 64-char hex.
    /// `handshake_client_stream_v9`'s `server_static_pub` parameter — see
    /// the module doc for why this field exists on this type today.
    pub server_pubkey_hex: String,
    /// Reused verbatim from `mirage_core::config::TransportCamouflage` (the
    /// same type `HopConfig.camouflage` already carries to clients). No new
    /// descriptor type invented for this.
    pub camouflage: TransportCamouflage,
}

/// The one customer-facing choice the owner's ruling grants: automatic, or a
/// named egress country.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtefactMode {
    FullAuto,
    /// ISO-3166 alpha-2, validated at [`EntranceArtefact::validate`] time.
    Country(String),
}

/// Every way [`EntranceArtefact::from_json`] and
/// [`EntranceArtefact::validate`] refuse — by name, never a panic.
#[derive(Debug, Error)]
pub enum ArtefactError {
    #[error("entrance artefact: unsupported version {found} (this client understands {expected})")]
    UnsupportedVersion { found: u32, expected: u32 },

    #[error("entrance artefact: mesh_id is empty")]
    EmptyMeshId,

    #[error("entrance artefact: no entrances — nothing to dial")]
    NoEntrances,

    #[error("entrance artefact: field '{field}' is not valid hex: {reason}")]
    MalformedHex { field: String, reason: String },

    #[error(
        "entrance artefact: field '{field}' must decode to {expected_bytes} bytes, got {got_bytes}"
    )]
    WrongHexLength { field: String, expected_bytes: usize, got_bytes: usize },

    #[error("entrance artefact: entrances[{index}].addr {addr:?} is not host:port")]
    BadEntranceAddr { index: usize, addr: String },

    #[error(
        "entrance artefact: mode country code {code:?} is not a 2-letter uppercase ISO-3166 alpha-2 code"
    )]
    BadCountryCode { code: String },

    #[error("entrance artefact: invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

fn check_hex_field(field: &str, value: &str, expected_bytes: usize) -> Result<(), ArtefactError> {
    let bytes = hex::decode(value).map_err(|e| ArtefactError::MalformedHex {
        field: field.to_owned(),
        reason: e.to_string(),
    })?;
    if bytes.len() != expected_bytes {
        return Err(ArtefactError::WrongHexLength {
            field: field.to_owned(),
            expected_bytes,
            got_bytes: bytes.len(),
        });
    }
    Ok(())
}

fn decode_hex32(field: &str, value: &str) -> Result<[u8; 32], ArtefactError> {
    let bytes = hex::decode(value).map_err(|e| ArtefactError::MalformedHex {
        field: field.to_owned(),
        reason: e.to_string(),
    })?;
    let got_bytes = bytes.len();
    bytes.try_into().map_err(|_| ArtefactError::WrongHexLength {
        field: field.to_owned(),
        expected_bytes: HEX32_BYTES,
        got_bytes,
    })
}

/// `true` for a plain `host:port` pair — non-empty host, port parses as a
/// `u16`. Not an IPv6-bracket parser — matches every published face today.
fn looks_like_host_port(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
        None => false,
    }
}

impl EntranceArtefact {
    /// Parse an artefact from its JSON form. Does NOT validate — call
    /// [`EntranceArtefact::validate`] before acting on the result.
    pub fn from_json(raw: &str) -> Result<Self, ArtefactError> {
        Ok(serde_json::from_str(raw)?)
    }

    /// Compact JSON form (no pretty-printing).
    pub fn to_json(&self) -> Result<String, ArtefactError> {
        Ok(serde_json::to_string(self)?)
    }

    /// Refuse a malformed artefact BY NAME. Only inspects `self` — never
    /// touches disk or the network.
    pub fn validate(&self) -> Result<(), ArtefactError> {
        if self.v != CURRENT_VERSION {
            return Err(ArtefactError::UnsupportedVersion { found: self.v, expected: CURRENT_VERSION });
        }
        if self.mesh_id.trim().is_empty() {
            return Err(ArtefactError::EmptyMeshId);
        }
        check_hex_field("private_key_hex", &self.private_key_hex, HEX32_BYTES)?;
        if self.entrances.is_empty() {
            return Err(ArtefactError::NoEntrances);
        }
        for (i, e) in self.entrances.iter().enumerate() {
            if !looks_like_host_port(&e.addr) {
                return Err(ArtefactError::BadEntranceAddr { index: i, addr: e.addr.clone() });
            }
            check_hex_field(
                &format!("entrances[{i}].server_pubkey_hex"),
                &e.server_pubkey_hex,
                HEX32_BYTES,
            )?;
        }
        if let ArtefactMode::Country(code) = &self.mode {
            let valid = code.len() == 2 && code.chars().all(|c| c.is_ascii_uppercase());
            if !valid {
                return Err(ArtefactError::BadCountryCode { code: code.clone() });
            }
        }
        Ok(())
    }

    /// The subscriber's persistent static X25519 secret, decoded once. Every
    /// entrance's [`crate::dial::StaticKeyProtocol`] is built from this SAME
    /// array — see `crate::assembly::SubscriberClient::build`.
    pub fn client_static_priv(&self) -> Result<[u8; 32], ArtefactError> {
        decode_hex32("private_key_hex", &self.private_key_hex)
    }
}

impl ArtefactEntrance {
    /// This face's box static X25519 public key, decoded once.
    pub fn server_pubkey(&self) -> Result<[u8; 32], ArtefactError> {
        decode_hex32("server_pubkey_hex", &self.server_pubkey_hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_artefact() -> EntranceArtefact {
        EntranceArtefact {
            v: CURRENT_VERSION,
            private_key_hex: "11".repeat(HEX32_BYTES),
            mesh_id: "MESH2".to_owned(),
            entrances: vec![
                ArtefactEntrance {
                    addr: "203.0.113.10:8443".to_owned(),
                    server_pubkey_hex: "22".repeat(HEX32_BYTES),
                    camouflage: TransportCamouflage::None,
                },
                ArtefactEntrance {
                    addr: "203.0.113.11:8443".to_owned(),
                    server_pubkey_hex: "33".repeat(HEX32_BYTES),
                    camouflage: TransportCamouflage::WebSocket {
                        path: "/ws".to_owned(),
                        host: "cdn.example.com".to_owned(),
                    },
                },
            ],
            mode: ArtefactMode::FullAuto,
            label: "pilot-1".to_owned(),
            issued_at_ms: 1_726_000_000_000,
        }
    }

    // ── parse / encode round-trip ────────────────────────────────────────

    #[test]
    fn round_trips_through_json() {
        let a = sample_artefact();
        let json = a.to_json().unwrap();
        let back = EntranceArtefact::from_json(&json).unwrap();
        assert_eq!(serde_json::to_value(&a).unwrap(), serde_json::to_value(&back).unwrap());
    }

    // ── good artefact ─────────────────────────────────────────────────────

    #[test]
    fn valid_artefact_passes() {
        assert!(sample_artefact().validate().is_ok());
    }

    // ── malformed ─────────────────────────────────────────────────────────

    #[test]
    fn validate_refuses_malformed_private_key_hex() {
        let mut a = sample_artefact();
        a.private_key_hex = "not-hex".to_owned();
        match a.validate() {
            Err(ArtefactError::MalformedHex { field, .. }) => assert_eq!(field, "private_key_hex"),
            other => panic!("expected MalformedHex, got {other:?}"),
        }
    }

    #[test]
    fn validate_refuses_wrong_length_server_pubkey_hex() {
        let mut a = sample_artefact();
        a.entrances[0].server_pubkey_hex = "aa".repeat(10); // valid hex, wrong length
        match a.validate() {
            Err(ArtefactError::WrongHexLength { field, expected_bytes, got_bytes }) => {
                assert_eq!(field, "entrances[0].server_pubkey_hex");
                assert_eq!(expected_bytes, HEX32_BYTES);
                assert_eq!(got_bytes, 10);
            }
            other => panic!("expected WrongHexLength, got {other:?}"),
        }
    }

    #[test]
    fn validate_refuses_bad_entrance_addr() {
        let mut a = sample_artefact();
        a.entrances[0].addr = "not-an-addr".to_owned();
        assert!(matches!(a.validate(), Err(ArtefactError::BadEntranceAddr { index: 0, .. })));
    }

    #[test]
    fn validate_refuses_unknown_version() {
        let mut a = sample_artefact();
        a.v = 2;
        assert!(matches!(
            a.validate(),
            Err(ArtefactError::UnsupportedVersion { found: 2, expected: 1 })
        ));
    }

    #[test]
    fn validate_refuses_bad_country_code() {
        let mut a = sample_artefact();
        a.mode = ArtefactMode::Country("Germany".to_owned());
        assert!(matches!(a.validate(), Err(ArtefactError::BadCountryCode { .. })));
    }

    #[test]
    fn validate_accepts_valid_country_code() {
        let mut a = sample_artefact();
        a.mode = ArtefactMode::Country("DE".to_owned());
        assert!(a.validate().is_ok());
    }

    // ── missing field (JSON parse, not validate) ────────────────────────────

    #[test]
    fn from_json_refuses_missing_required_field() {
        // `entrances` omitted entirely — required, no #[serde(default)].
        let raw = serde_json::json!({
            "v": CURRENT_VERSION,
            "private_key_hex": "11".repeat(HEX32_BYTES),
            "mesh_id": "MESH2",
            "mode": "full_auto",
            "label": "pilot-1",
            "issued_at_ms": 1_726_000_000_000u64,
        })
        .to_string();
        assert!(matches!(EntranceArtefact::from_json(&raw), Err(ArtefactError::Json(_))));
    }

    // ── refusal when the artefact carries no usable entrance ───────────────

    #[test]
    fn validate_refuses_empty_entrances() {
        let mut a = sample_artefact();
        a.entrances.clear();
        assert!(matches!(a.validate(), Err(ArtefactError::NoEntrances)));
    }

    // ── key/pubkey decode ────────────────────────────────────────────────

    #[test]
    fn client_static_priv_decodes_to_the_exact_bytes() {
        let a = sample_artefact();
        let key = a.client_static_priv().unwrap();
        assert_eq!(key, [0x11u8; HEX32_BYTES]);
    }

    #[test]
    fn server_pubkey_decodes_to_the_exact_bytes() {
        let a = sample_artefact();
        let pk = a.entrances[0].server_pubkey().unwrap();
        assert_eq!(pk, [0x22u8; HEX32_BYTES]);
    }
}
