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
//! exists anywhere on the artefact as that companion plan drafted it. At the
//! time this closed the gap locally, no crate in this workspace had landed
//! `EntranceArtefact` yet (grepped empty) — so this type was free to close
//! it rather than parse a shape that could not be dialed.
//!
//! # Reconciled with the panel's own type (2026-09-22)
//!
//! `mirage2panel/crates/panel/src/entrance.rs` has since landed its OWN
//! `EntranceArtefact` — written independently there too, for the same
//! reason (it grepped ITS tree and found nothing). The two now agree field
//! for field: the panel renamed its per-entrance key field from
//! `pubkey_hex: Option<String>` to `server_pubkey_hex: String`, matching
//! this crate's already-required field exactly, and moved the
//! optionality that genuinely exists at its source (opbox may not yet hold
//! a Monitor identity in escrow for a given box) to a filter at
//! artefact-build time rather than an `Option` on the wire — a face this
//! crate could never dial is never shipped in the first place. Neither
//! crate depends on the other: the panel's own contract keeps it free of
//! `mirage-core` and every other mesh crate, so the two types can only
//! agree by JSON shape, not by sharing a Rust type.
//! `tests::parses_a_panel_produced_artefact_field_for_field` is the proof —
//! it parses the exact literal the panel's own
//! `entrance::tests::panel_output_matches_the_client_wire_contract`
//! serialises through its REAL `Serialize` impl.
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

use mirage_core::mirage::wire::{Protocol, TransportCamouflage};
use serde::de::{self, Deserializer};
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
///
/// `Debug` prints the protocol's name, not its body. WireGuard and
/// Shadowsocks carry key material.
#[derive(Clone, Serialize)]
pub struct ArtefactEntrance {
    /// `host:port` where the protocol ends (the terminator).
    pub addr: String,
    /// Optional `host:port` relay. Absent dials [`Self::addr`] directly.
    /// The protocol still ends at `addr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// The box's static X25519 public key for THIS face, 64-char hex.
    /// `handshake_client_stream_v9`'s `server_static_pub` parameter — see
    /// the module doc for why this field exists on this type today.
    pub server_pubkey_hex: String,
    /// The link this face speaks. The port is [`Self::addr`], not a field
    /// of the protocol. Dial code that still needs the old mask calls
    /// [`Protocol::to_mask`].
    ///
    /// Files written before this field existed name the same value
    /// `camouflage`. Both shapes are read. New files write `protocol`.
    pub protocol: Protocol,
}

impl<'de> Deserialize<'de> for ArtefactEntrance {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let addr = value
            .get("addr")
            .and_then(|item| item.as_str())
            .ok_or_else(|| de::Error::missing_field("addr"))?
            .to_owned();
        let relay = match value.get("relay") {
            None | Some(serde_json::Value::Null) => None,
            Some(item) => Some(
                item.as_str()
                    .ok_or_else(|| de::Error::custom("relay is not a string"))?
                    .to_owned(),
            ),
        };
        let server_pubkey_hex = value
            .get("server_pubkey_hex")
            .and_then(|item| item.as_str())
            .ok_or_else(|| de::Error::missing_field("server_pubkey_hex"))?
            .to_owned();
        let protocol = if let Some(item) = value.get("protocol") {
            serde_json::from_value(item.clone()).map_err(de::Error::custom)?
        } else if let Some(item) = value.get("camouflage") {
            let mask: TransportCamouflage =
                serde_json::from_value(item.clone()).map_err(de::Error::custom)?;
            Protocol::try_from(&mask).map_err(de::Error::custom)?
        } else {
            return Err(de::Error::missing_field("protocol"));
        };
        Ok(Self {
            addr,
            relay,
            server_pubkey_hex,
            protocol,
        })
    }
}

impl std::fmt::Debug for ArtefactEntrance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtefactEntrance")
            .field("addr", &self.addr)
            .field("relay", &self.relay)
            .field("server_pubkey_hex", &self.server_pubkey_hex)
            .field("protocol", &self.protocol.label())
            .finish()
    }
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

    #[error("entrance artefact: entrances[{index}].relay {relay:?} is not host:port")]
    BadEntranceRelay { index: usize, relay: String },

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
            if let Some(relay) = &e.relay {
                if !looks_like_host_port(relay) {
                    return Err(ArtefactError::BadEntranceRelay { index: i, relay: relay.clone() });
                }
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

/// The two kinds of artefact this client understands.
///
/// The VPN artefact names ONE box — its faces. The onion artefact names a
/// CHAIN — the hops the client walks itself. They are told apart by the
/// `kind` field: `"onion"` is the onion artefact, and **its absence means
/// the VPN one** — the fleet's own shape, which this crate does not change
/// by a byte.
#[derive(Debug, Clone)]
pub enum DeliveryArtefact {
    /// The VPN artefact: the faces of one box.
    Vpn(EntranceArtefact),
    /// The onion artefact: the hops of one chain.
    Onion(crate::onion_artefact::OnionArtefact),
}

/// Every way reading an artefact of either kind can fail, by name.
#[derive(Debug, Error)]
pub enum DeliveryArtefactError {
    #[error(transparent)]
    Vpn(#[from] ArtefactError),

    #[error(transparent)]
    Onion(#[from] crate::onion_artefact::OnionArtefactError),

    #[error("the artefact names an unknown kind {0:?}")]
    UnknownKind(String),
}

impl DeliveryArtefact {
    /// Read an artefact of either kind.
    ///
    /// The discriminant is read first: `"kind": "onion"` means the onion
    /// artefact; an artefact without `kind` is the VPN one.
    pub fn from_json(raw: &str) -> Result<Self, DeliveryArtefactError> {
        let value: serde_json::Value = serde_json::from_str(raw).map_err(ArtefactError::Json)?;
        let kind = value.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        if kind == crate::onion_artefact::ONION_KIND {
            return Ok(Self::Onion(crate::onion_artefact::OnionArtefact::from_json(
                raw,
            )?));
        }
        if !kind.is_empty() {
            return Err(DeliveryArtefactError::UnknownKind(kind.to_owned()));
        }
        Ok(Self::Vpn(EntranceArtefact::from_json(raw)?))
    }

    /// Validate whichever artefact this is.
    pub fn validate(&self) -> Result<(), DeliveryArtefactError> {
        match self {
            Self::Vpn(artefact) => artefact.validate().map_err(DeliveryArtefactError::Vpn),
            Self::Onion(artefact) => artefact.validate().map_err(DeliveryArtefactError::Onion),
        }
    }

    /// The client's own persistent static key, whichever artefact this is.
    pub fn client_static_priv(&self) -> Result<[u8; 32], DeliveryArtefactError> {
        match self {
            Self::Vpn(artefact) => artefact
                .client_static_priv()
                .map_err(DeliveryArtefactError::Vpn),
            Self::Onion(artefact) => artefact
                .client_static_priv()
                .map_err(DeliveryArtefactError::Onion),
        }
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
                    relay: None,
                    server_pubkey_hex: "22".repeat(HEX32_BYTES),
                    protocol: Protocol::Mirage {
                        carrier: mirage_core::mirage::wire::MirageCarrier::Tcp,
                        camouflage: mirage_core::mirage::wire::MirageCamouflage::None,
                    },
                },
                ArtefactEntrance {
                    addr: "203.0.113.11:8443".to_owned(),
                    relay: None,
                    server_pubkey_hex: "33".repeat(HEX32_BYTES),
                    protocol: Protocol::WebSocket {
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
    fn validate_refuses_bad_relay() {
        let mut a = sample_artefact();
        a.entrances[0].relay = Some("not-an-addr".to_owned());
        assert!(matches!(a.validate(), Err(ArtefactError::BadEntranceRelay { index: 0, .. })));
    }

    #[test]
    fn validate_accepts_a_relay_and_round_trips_it() {
        let mut a = sample_artefact();
        a.entrances[0].relay = Some("198.51.100.8:443".to_owned());
        assert!(a.validate().is_ok());
        let back = EntranceArtefact::from_json(&a.to_json().unwrap()).unwrap();
        assert_eq!(back.entrances[0].relay.as_deref(), Some("198.51.100.8:443"));
        assert!(back.entrances[1].relay.is_none());
    }

    #[test]
    fn debug_prints_relay_like_addr_and_not_the_protocol_body() {
        let entrance = ArtefactEntrance {
            addr: "203.0.113.10:8443".to_owned(),
            relay: Some("198.51.100.8:443".to_owned()),
            server_pubkey_hex: "22".repeat(HEX32_BYTES),
            protocol: Protocol::Shadowsocks {
                method: "aes-256-gcm".to_owned(),
                password: "super-secret-password".to_owned(),
            },
        };
        let rendered = format!("{entrance:?}");
        assert!(rendered.contains("203.0.113.10:8443"), "{rendered}");
        assert!(rendered.contains("198.51.100.8:443"), "{rendered}");
        assert!(rendered.contains("shadowsocks"), "{rendered}");
        assert!(!rendered.contains("super-secret-password"), "{rendered}");
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

    // ── wire compatibility with the panel ───────────────────────────────

    /// The wire contract with `mirage-panel` — see this module's own doc,
    /// "Reconciled with the panel's own type", and
    /// `mirage2panel/crates/panel/src/entrance.rs`'s
    /// `tests::panel_output_matches_the_client_wire_contract`, which
    /// serialises the IDENTICAL literal values below through the panel's
    /// REAL `Serialize` impl (kept in sync by hand — the two crates share
    /// no dependency, so a matching literal is the only handle either side
    /// has on the other's shape). This test is the consuming half: it
    /// parses that exact shape through this crate's REAL `Deserialize` +
    /// `validate()`, field for field, including the camouflage passthrough
    /// decoding into a real typed variant and the (now required, never
    /// optional) per-entrance public key.
    #[test]
    fn parses_a_panel_produced_artefact_field_for_field() {
        let raw = serde_json::json!({
            "v": 1,
            "private_key_hex": "11".repeat(32),
            "mesh_id": "22".repeat(32),
            "entrances": [{
                "addr": "203.0.113.9:443",
                "server_pubkey_hex": "33".repeat(32),
                "protocol": {"type": "tls", "sni": "entrance.example.com", "alpn": ["h2"]}
            }],
            "mode": "full_auto",
            "label": "Ada",
            "issued_at_ms": 1_726_000_000_000u64
        })
        .to_string();

        let artefact = EntranceArtefact::from_json(&raw).expect("panel-shaped JSON must parse");
        artefact.validate().expect("panel-shaped artefact must validate");

        assert_eq!(artefact.v, CURRENT_VERSION);
        assert_eq!(artefact.private_key_hex, "11".repeat(32));
        assert_eq!(artefact.mesh_id, "22".repeat(32));
        assert_eq!(artefact.mode, ArtefactMode::FullAuto);
        assert_eq!(artefact.label, "Ada");
        assert_eq!(artefact.issued_at_ms, 1_726_000_000_000);

        assert_eq!(artefact.entrances.len(), 1);
        let entrance = &artefact.entrances[0];
        assert_eq!(entrance.addr, "203.0.113.9:443");
        assert!(entrance.relay.is_none(), "an artefact with no relay field still deserializes");
        assert_eq!(entrance.server_pubkey_hex, "33".repeat(32));
        assert_eq!(
            entrance.server_pubkey().expect("decode server pubkey"),
            [0x33u8; HEX32_BYTES],
            "the panel's server_pubkey_hex must decode to the exact bytes it sent"
        );
        assert_eq!(
            entrance.protocol,
            Protocol::Tls { sni: "entrance.example.com".to_owned(), alpn: vec!["h2".to_owned()] },
            "the entrance protocol must decode to the typed link"
        );

        let legacy = serde_json::json!({
            "v": 1,
            "private_key_hex": "11".repeat(32),
            "mesh_id": "22".repeat(32),
            "entrances": [{
                "addr": "203.0.113.9:443",
                "server_pubkey_hex": "33".repeat(32),
                "camouflage": {"type": "quic", "alpn": ["h3"], "bind_addr": "0.0.0.0:443"}
            }],
            "mode": "full_auto",
            "label": "Ada",
            "issued_at_ms": 1_726_000_000_000u64
        })
        .to_string();
        let old = EntranceArtefact::from_json(&legacy).expect("a camouflage entrance still parses");
        assert!(matches!(old.entrances[0].protocol, Protocol::Quic { .. }));

        assert_eq!(
            artefact.client_static_priv().expect("decode client private key"),
            [0x11u8; HEX32_BYTES]
        );
    }
}
