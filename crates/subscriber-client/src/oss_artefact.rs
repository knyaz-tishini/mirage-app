//! Building the delivery artefact from a self-assembled box's publication.
//!
//! # The seam this closes
//!
//! The fleet's box is described to the client by the **panel**: the panel
//! issues an [`EntranceArtefact`] and the client adopts it. A self-assembled
//! box has no panel — the operator IS the client — so the operator has to
//! build the very same artefact itself, out of what the box published.
//!
//! That is what this module does. The result is an ordinary
//! [`EntranceArtefact`]: the client's data plane cannot tell where it came
//! from, and does not need to. **One client, two sources of the artefact.**
//!
//! # What the operator supplies
//!
//! - the box's publication ([`OssProposal`]) — read over SSH from
//!   `mirage-vpn propose`;
//! - the client's own key pair — the operator generates it, because in this
//!   mode the client is the subscriber and there is nobody else to issue one;
//! - the host the operator reached the box on — the publication carries ports
//!   and masks, never the address the operator dialled.
//!
//! # The client's key and the box's allowlist
//!
//! The box admits peers by their **public** key, in **base64** (that is the
//! shape its peer table takes). The artefact carries keys in **hex** (that is
//! the shape the wire contract takes). Both come from the same key pair, and
//! [`client_public_key_b64`] is the bridge.

use mirage_client_chain::oss::OssProposal;
use mirage_core::config::TransportCamouflage;
use mirage_core::crypto::KeyPair;
use mirage_core::mirage::wire::Protocol;
use thiserror::Error;

use crate::artefact::{ArtefactEntrance, ArtefactError, ArtefactMode, EntranceArtefact};

/// Every way building an artefact from a publication can fail, by name.
#[derive(Debug, Error)]
pub enum OssArtefactError {
    /// The box published no face a client could dial.
    #[error("the box published no face a client can dial")]
    NoDialableFace,

    /// A face's mask is not a camouflage this client understands.
    #[error("the face's mask is not a camouflage: {0}")]
    BadMask(String),

    /// The built artefact does not pass the wire contract's own validation.
    #[error("the artefact does not validate: {0}")]
    Invalid(#[from] ArtefactError),
}

/// Build the artefact from a box's publication.
///
/// The result is validated before it is returned: an artefact that does not
/// pass [`EntranceArtefact::validate`] is never handed to the data plane.
pub fn build(
    proposal: &OssProposal,
    client_keypair: &KeyPair,
    host: &str,
    label: &str,
    issued_at_ms: u64,
) -> Result<EntranceArtefact, OssArtefactError> {
    let mut entrances = Vec::new();
    for face in proposal.dialable() {
        let Some(mask) = &face.mask else {
            continue;
        };
        let camouflage: TransportCamouflage = serde_json::from_value(mask.clone())
            .map_err(|err| OssArtefactError::BadMask(err.to_string()))?;
        // The configuration names the protocol the session runs on, and this
        // is where that name is given. A datagram carrier cannot carry it:
        // the session is a mux, it does not retransmit, and the transport
        // under it authenticates every frame with a counter -- one lost
        // datagram ends the session.
        //
        // Measured on the debug boxes, same box, same 20 MiB download: the
        // WireGuard face carried 335 KiB and the session dropped with
        // "authentication failed" after ten seconds; the QUIC face carried
        // all 20 MiB at 3.2 MB/s without a single error.
        if !camouflage.carries_reliable_stream() {
            continue;
        }
        let protocol = Protocol::try_from(&camouflage)
            .map_err(OssArtefactError::BadMask)?;
        entrances.push(ArtefactEntrance {
            addr: format!("{host}:{}", face.port),
            relay: None,
            server_pubkey_hex: proposal.server_public_key_hex.clone(),
            protocol,
        });
    }
    if entrances.is_empty() {
        return Err(OssArtefactError::NoDialableFace);
    }

    let artefact = EntranceArtefact {
        v: crate::artefact::CURRENT_VERSION,
        private_key_hex: hex::encode(client_keypair.secret.to_bytes()),
        mesh_id: proposal.mesh_id.clone(),
        entrances,
        mode: ArtefactMode::FullAuto,
        label: label.to_owned(),
        issued_at_ms,
    };
    artefact.validate()?;
    Ok(artefact)
}

/// The client's own public key, base64 — the shape a box's peer table takes.
///
/// The operator puts this into the box's config before the box starts: the
/// box admits peers by their public key, and in self-assembly the client is
/// the peer.
///
/// A minimal encoder, standard alphabet with padding — the same choice
/// `mirage-client-core` makes for its own base64 need, so this tree pulls in
/// no extra dependency for it.
pub fn client_public_key_b64(keypair: &KeyPair) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = keypair.public.as_bytes();
    let mut out = Vec::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let combined = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((combined >> 18) & 0x3f) as usize]);
        out.push(TABLE[((combined >> 12) & 0x3f) as usize]);
        out.push(if chunk.len() > 1 {
            TABLE[((combined >> 6) & 0x3f) as usize]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(combined & 0x3f) as usize]
        } else {
            b'='
        });
    }
    String::from_utf8(out).expect("base64 output is always valid ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The publication a real OSS box printed on the stand -- verbatim.
    ///
    /// The box serves QUIC only: the MirageKVIC carrier is its one face.
    const FROM_THE_BOX: &str = r#"{
      "v": 1,
      "server_public_key_hex": "f0e1d3d2b6afca2614fee067738ea2b25cf2c9f1b819e3eea3cd64da1f505901",
      "mesh_id": "mirage-vpn",
      "faces": [
        {
          "port": 41444,
          "transport": "udp",
          "mask": {
            "type": "quic",
            "alpn": [ "h3" ],
            "bind_addr": "0.0.0.0:41444"
          },
          "why": "UDP - the MirageKVIC carrier."
        }
      ]
    }"#;

    #[test]
    fn an_artefact_is_built_from_the_boxs_publication() {
        let proposal = OssProposal::parse(FROM_THE_BOX).expect("read");
        let keypair = KeyPair::generate();
        let artefact = build(&proposal, &keypair, "203.0.113.9", "self-built", 1_726_000_000_000)
            .expect("the artefact must build");

        // It passes the wire contract's own validation.
        artefact.validate().expect("it must validate");

        assert_eq!(artefact.entrances.len(), 1);
        assert_eq!(artefact.entrances[0].addr, "203.0.113.9:41444");
        assert_eq!(
            artefact.entrances[0].server_pubkey_hex,
            "f0e1d3d2b6afca2614fee067738ea2b25cf2c9f1b819e3eea3cd64da1f505901"
        );
        assert_eq!(
            artefact.entrances[0].protocol,
            Protocol::Quic { alpn: vec!["h3".to_owned()] },
            "the publication's quic mask becomes the quic protocol"
        );
    }

    #[test]
    fn the_clients_own_key_is_the_one_the_artefact_carries() {
        let proposal = OssProposal::parse(FROM_THE_BOX).expect("read");
        let keypair = KeyPair::generate();
        let artefact = build(&proposal, &keypair, "203.0.113.9", "self-built", 0).expect("build");
        assert_eq!(
            artefact.private_key_hex,
            hex::encode(keypair.secret.to_bytes()),
            "the artefact must carry the operator's own key"
        );
        assert_eq!(
            artefact.client_static_priv().expect("decode"),
            keypair.secret.to_bytes()
        );
    }

    #[test]
    fn the_mesh_id_is_the_boxs_own() {
        let proposal = OssProposal::parse(FROM_THE_BOX).expect("read");
        let keypair = KeyPair::generate();
        let artefact = build(&proposal, &keypair, "203.0.113.9", "self-built", 0).expect("build");
        assert_eq!(artefact.mesh_id, "mirage-vpn");
    }

    #[test]
    fn a_box_with_no_dialable_face_is_refused_by_name() {
        let raw = r#"{
          "v": 1,
          "server_public_key_hex": "f0e1d3d2b6afca2614fee067738ea2b25cf2c9f1b819e3eea3cd64da1f505901",
          "mesh_id": "mirage-vpn",
          "faces": [ { "port": 41445, "transport": "tcp", "mask": null, "why": "the tube" } ]
        }"#;
        let proposal = OssProposal::parse(raw).expect("read");
        let keypair = KeyPair::generate();
        let err = build(&proposal, &keypair, "203.0.113.9", "self-built", 0)
            .expect_err("a tube alone is not dialable");
        assert!(matches!(err, OssArtefactError::NoDialableFace));
    }

    #[test]
    fn a_mask_the_client_cannot_read_is_refused_by_name() {
        let raw = r#"{
          "v": 1,
          "server_public_key_hex": "f0e1d3d2b6afca2614fee067738ea2b25cf2c9f1b819e3eea3cd64da1f505901",
          "mesh_id": "mirage-vpn",
          "faces": [ { "port": 41443, "transport": "udp", "mask": { "type": "not_a_mask" }, "why": "?" } ]
        }"#;
        let proposal = OssProposal::parse(raw).expect("read");
        let keypair = KeyPair::generate();
        let err = build(&proposal, &keypair, "203.0.113.9", "self-built", 0)
            .expect_err("an unknown mask cannot be dialled");
        assert!(matches!(err, OssArtefactError::BadMask(_)));
    }

    #[test]
    fn the_public_key_is_base64_for_the_boxs_peer_table() {
        let keypair = KeyPair::generate();
        let encoded = client_public_key_b64(&keypair);
        assert_eq!(encoded.len(), 44, "32 bytes of key are 44 base64 characters");
        assert!(encoded.ends_with('='), "the padding is there");
        assert!(encoded
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }

    #[test]
    fn the_artefact_is_json_the_client_can_parse_back() {
        let proposal = OssProposal::parse(FROM_THE_BOX).expect("read");
        let keypair = KeyPair::generate();
        let artefact = build(&proposal, &keypair, "203.0.113.9", "self-built", 0).expect("build");
        let json = artefact.to_json().expect("json");
        let back = EntranceArtefact::from_json(&json).expect("the client parses its own artefact");
        back.validate().expect("and validates it");
        assert_eq!(back.entrances.len(), artefact.entrances.len());
    }
}
