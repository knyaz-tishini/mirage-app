//! What a self-assembled box publishes about itself.
//!
//! # Why this lives beside the fleet's proposal
//!
//! The fleet's box publishes its faces too — `m1r4g3-server propose` writes
//! them, and [`crate::provision::ProposedFace`] reads them. A self-assembled
//! box has no such command: it is the OSS `mirage-vpn`, and it publishes
//! itself with `mirage-vpn propose`.
//!
//! The two shapes are the same idea and are kept apart on purpose:
//!
//! - the fleet's face carries its mask as an **opaque JSON string** — the
//!   client writes it back into `server.env` without understanding it;
//! - this face carries the mask as **opaque JSON** too, for the same reason:
//!   this crate does not depend on `mirage-core`, so it cannot name the
//!   camouflage type. The crate that does — `mirage-subscriber-client` —
//!   decodes it into the real typed variant.
//!
//! So the operator reads a box's publication with this module, and the
//! client's data plane turns it into an entrance with the same code it
//! already uses for a fleet entrance.

use serde::{Deserialize, Serialize};

/// The version of the proposal shape this crate understands.
pub const PROPOSAL_VERSION: u32 = 1;

/// What a self-assembled box publishes about itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OssProposal {
    /// The version of this shape.
    pub v: u32,
    /// The box's own static public key, hex — the key a client verifies it
    /// is talking to *this* box and not something standing in front of it.
    pub server_public_key_hex: String,
    /// The id both ends bake into the handshake.
    pub mesh_id: String,
    /// The faces the box can be reached on.
    pub faces: Vec<OssFace>,
}

/// One door of the box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OssFace {
    /// The port the face sits on.
    pub port: u16,
    /// `udp` or `tcp`.
    pub transport: String,
    /// The camouflage the face wears, as JSON. `None` for a door that is not
    /// a camouflage axis — the SOCKS5 tube is a pipe, not a disguise.
    pub mask: Option<serde_json::Value>,
    /// Why the face is offered, in the operator's language.
    pub why: String,
}

impl OssProposal {
    /// Parse the publication a box printed.
    ///
    /// Refuses by name, never by panic: a box that printed something else is
    /// a box the operator has to read, not a crash.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let proposal: OssProposal = serde_json::from_str(raw)
            .map_err(|err| format!("the proposal is not valid JSON: {err}"))?;
        if proposal.v != PROPOSAL_VERSION {
            return Err(format!(
                "the proposal is version {}, this client understands {PROPOSAL_VERSION}",
                proposal.v
            ));
        }
        Ok(proposal)
    }

    /// The faces a client can dial: the ones that wear a camouflage.
    pub fn dialable(&self) -> impl Iterator<Item = &OssFace> {
        self.faces.iter().filter(|face| face.mask.is_some())
    }

    /// The faces that are not camouflage axes — the SOCKS5 tube.
    pub fn pipes(&self) -> impl Iterator<Item = &OssFace> {
        self.faces.iter().filter(|face| face.mask.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The publication a real OSS box printed on the stand — verbatim, so the
    /// parser is checked against what the box actually says, not against an
    /// idealised shape.
    const FROM_THE_BOX: &str = r#"{
      "v": 1,
      "server_public_key_hex": "f0e1d3d2b6afca2614fee067738ea2b25cf2c9f1b819e3eea3cd64da1f505901",
      "mesh_id": "mirage-vpn",
      "faces": [
        {
          "port": 41443,
          "transport": "udp",
          "mask": {
            "type": "wire_guard",
            "magic_byte": 1,
            "junk_count": 4,
            "junk_min": 10,
            "junk_max": 20,
            "bind_addr": "0.0.0.0:41443"
          },
          "why": "UDP - the MirageVG carrier."
        }
      ]
    }"#;

    #[test]
    fn the_publication_a_box_prints_is_read() {
        let proposal = OssProposal::parse(FROM_THE_BOX).expect("the box's publication must read");
        assert_eq!(proposal.v, PROPOSAL_VERSION);
        assert_eq!(proposal.mesh_id, "mirage-vpn");
        assert_eq!(proposal.server_public_key_hex.len(), 64);
        assert_eq!(proposal.faces.len(), 1);
        assert_eq!(proposal.faces[0].port, 41443);
        assert_eq!(proposal.faces[0].transport, "udp");
    }

    #[test]
    fn the_mask_stays_opaque_here_and_is_still_the_mask() {
        let proposal = OssProposal::parse(FROM_THE_BOX).expect("read");
        let mask = proposal.faces[0].mask.as_ref().expect("the face wears a mask");
        assert_eq!(mask["type"], "wire_guard");
        assert_eq!(mask["magic_byte"], 1);
    }

    #[test]
    fn a_face_with_a_mask_is_dialable_and_a_face_without_is_not() {
        let raw = r#"{
          "v": 1,
          "server_public_key_hex": "f0e1d3d2b6afca2614fee067738ea2b25cf2c9f1b819e3eea3cd64da1f505901",
          "mesh_id": "mirage-vpn",
          "faces": [
            { "port": 41443, "transport": "udp", "mask": { "type": "quic" }, "why": "carrier" },
            { "port": 41445, "transport": "tcp", "mask": null, "why": "the tube" }
          ]
        }"#;
        let proposal = OssProposal::parse(raw).expect("read");
        let dialable: Vec<u16> = proposal.dialable().map(|f| f.port).collect();
        let pipes: Vec<u16> = proposal.pipes().map(|f| f.port).collect();
        assert_eq!(dialable, vec![41443]);
        assert_eq!(pipes, vec![41445]);
    }

    #[test]
    fn a_proposal_of_another_version_is_refused_by_name() {
        let raw = r#"{"v": 7, "server_public_key_hex": "", "mesh_id": "x", "faces": []}"#;
        let err = OssProposal::parse(raw).expect_err("version 7 is not understood");
        assert!(err.contains('7'), "the error must name the version: {err}");
    }

    #[test]
    fn a_proposal_that_is_not_json_is_refused_by_name() {
        let err = OssProposal::parse("not json at all").expect_err("not json");
        assert!(err.contains("not valid JSON"), "{err}");
    }
}
