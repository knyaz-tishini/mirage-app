//! Putting a chain of boxes together, box by box.
//!
//! # The order matters
//!
//! The boxes are installed FIRST, each on its own, and only then does the
//! chain get written into them. The reason is what a chain hop needs to know
//! about its neighbour: the next box's address, its wire key and the mask it
//! wears on the wire. None of that exists until the next box is up and
//! publishing — a box that is asked to relay to a neighbour nobody has
//! installed yet has nothing to relay to.
//!
//! So: install every box, read what each of them published, build the chain
//! from that, and then write the chain into every box and bring them back up
//! with it.
//!
//! # The edge and the hops
//!
//! The FIRST box of the chain is the EDGE — the box the customer talks to.
//! It owns the chain and dials it: the edge dials the first HOP, which is
//! the second box. The edge is not a hop of its own chain, so the chain's
//! hop list starts at the second box, and the edge alone is told where to
//! dial it.
//!
//! # The keys
//!
//! Every box gets its own state directory: its own client key, its own
//! host-key pin, its own link. A chain of two boxes that shared one
//! directory would have the second box's install overwrite the first box's
//! pin — and the first box's pin is the thing that catches a swapped-out
//! machine.
//!
//! The chain itself has one keypair of its own, minted here and shared by
//! every box of the chain: the edge signs the chain leg's credential with
//! the secret, and every hop verifies that signature against the public
//! key. The mesh id is the edge's own, read from the edge's link — the
//! customer's link carries it, so the chain's legs have to match it.

use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rand::RngCore;

use crate::chain::{Chain, ChainBox, ChainFace, ChainKeys, ChainRole};
use crate::provision::{self, InstallBus, Job, Outcome};

/// The camouflage types that speak UDP rather than TCP.
///
/// The chain's leg between two boxes rides TCP: the leg is a Mirage session
/// inside the camouflage, and the camouflage is what the leg wears. A
/// datagram camouflage cannot carry it.
const UDP_CAMOUFLAGE_TYPES: &[&str] = &[
    "quic",
    "http3",
    "web_transport",
    "wire_guard",
    "raw_udp",
];

/// How long a mesh id is, in hex characters.
const MESH_ID_HEX_LEN: usize = 64;

/// One box to put into a chain.
#[derive(Debug, Clone)]
pub struct ChainRequest {
    /// The box's address.
    pub host: String,
    /// The SSH port.
    pub port: u16,
    /// The login.
    pub user: String,
    /// The password — used once, to install the key, and dropped after.
    pub password: String,
}

/// One box of the chain, after it was installed.
#[derive(Debug, Clone)]
pub struct InstalledBox {
    /// The box's address.
    pub host: String,
    /// What the box does in the chain: the edge owns it, a hop relays.
    pub role: ChainRole,
    /// How many doors the box published.
    pub faces: usize,
    /// The door the box published for the chain: the hop's door, where the
    /// chain leg knocks, and the edge's own door, where the customer does.
    pub face: ChainFace,
}

/// What the chain turned out to be.
#[derive(Debug, Clone)]
pub struct ChainOutcome {
    /// The chain, as the client built it.
    pub chain: Chain,
    /// The link the client dials — the EDGE's own.
    ///
    /// A client that joins the chain walks the same road as everybody else,
    /// so it starts where the chain starts: at the edge.
    pub link: String,
    /// Every box, in chain order: the edge first, then the hops.
    pub boxes: Vec<InstalledBox>,
}

/// The box's own published link, as far as the chain needs it.
///
/// The full type lives in the subscriber client — the client that DIALS. The
/// chain does not dial: it reads three fields of what a box published and
/// writes them into the next box's configuration. The client build does not
/// carry the dial machinery, so the chain reads what it needs and nothing
/// else.
#[derive(Debug, Clone, serde::Deserialize)]
struct BoxArtefact {
    /// The box's own mesh id, hex.
    ///
    /// The chain shares it: the customer's link carries the edge's mesh id,
    /// and every box of the chain has to carry the same one.
    #[serde(default)]
    mesh_id: String,
    /// Every door the box published.
    entrances: Vec<ArtefactEntrance>,
}

/// One door of a published box.
#[derive(Debug, Clone, serde::Deserialize)]
struct ArtefactEntrance {
    /// `host:port` the door is on.
    addr: String,
    /// The box's wire key, hex — the key the chain leg verifies the box by.
    server_pubkey_hex: String,
    /// The mask the door wears, exactly as the box serialized it.
    ///
    /// It is kept as raw JSON and written back unchanged: the client does
    /// not need to understand the mask, and a mask that travels through the
    /// client as text cannot be changed by accident on the way.
    camouflage: serde_json::Value,
}

/// Install a chain: every box, then the chain itself.
///
/// `base_env` is what every box needs for itself — the stack, the protocols,
/// the ports — and the chain is added to it.
pub async fn install_chain(
    chain_id: &str,
    boxes: &[ChainRequest],
    base_env: &str,
    bus: &InstallBus,
) -> Result<ChainOutcome, String> {
    if boxes.is_empty() {
        return Err("a chain needs at least one box".to_owned());
    }

    // Step one: every box, on its own, with its own keys and its own pins.
    let mut links = Vec::with_capacity(boxes.len());
    let mut states = Vec::with_capacity(boxes.len());
    for request in boxes {
        let state = box_state_dir(request)?;
        let job = Job {
            host: request.host.clone(),
            port: request.port,
            user: request.user.clone(),
            password: request.password.clone(),
            stack: String::new(),
            env: base_env.to_owned(),
            ask_faces: false,
            state: Some(state.clone()),
        };
        let outcome = provision::install(job, bus).await?;
        let link = match outcome {
            Outcome::Link { link, .. } => link,
            Outcome::Faces { .. } => {
                return Err(format!(
                    "{}: the box only offered its faces — install it on its own first",
                    request.host
                ))
            }
        };
        links.push(link);
        states.push(state);
    }

    // Step two: what each box published becomes the chain.
    let mut artefacts = Vec::with_capacity(boxes.len());
    for (request, link) in boxes.iter().zip(links.iter()) {
        let artefact: BoxArtefact = serde_json::from_str(link)
            .map_err(|e| format!("{}: the box's link is not readable: {e}", request.host))?;
        artefacts.push(artefact);
    }

    // The chain's own key material: ONE operator keypair for the whole
    // chain, and the edge's mesh id — the customer's link carries it, so the
    // chain's legs have to match it.
    let keys = chain_keys(&artefacts[0])?;

    // The first box is the EDGE: it owns the chain and dials it, and it is
    // not a hop of its own chain. Every box after it is a hop, and the
    // chain's leg knocks on the hop's door.
    let mut chain_boxes = Vec::with_capacity(boxes.len());
    for (index, (request, artefact)) in boxes.iter().zip(artefacts.iter()).enumerate() {
        let face = if index == 0 {
            pick_edge_face(artefact)
        } else {
            pick_face(artefact)
        };
        let face = face.ok_or_else(|| {
            if index == 0 {
                format!("{}: the box published no door at all", request.host)
            } else {
                format!(
                    "{}: the box published no TCP door for the chain leg to use",
                    request.host
                )
            }
        })?;
        chain_boxes.push(ChainBox {
            host: request.host.clone(),
            user: request.user.clone(),
            face,
        });
    }

    let chain = Chain {
        chain_id: chain_id.to_owned(),
        boxes: chain_boxes,
        keys,
    };

    let installed: Vec<InstalledBox> = chain
        .boxes
        .iter()
        .zip(artefacts.iter())
        .enumerate()
        .map(|(index, (chain_box, artefact))| InstalledBox {
            host: chain_box.host.clone(),
            role: chain.role_of(index),
            faces: artefact.entrances.len(),
            face: chain_box.face.clone(),
        })
        .collect();

    // Step three: the chain goes into every box, and every box comes back up
    // with it. A box that kept the old configuration would be a box that
    // never relays — and the chain would look installed while carrying the
    // client straight past the hop it was built for.
    for (index, (request, state)) in boxes.iter().zip(states.iter()).enumerate() {
        let env = chain.server_env(index, base_env)?;
        provision::reconfigure(&request.host, request.port, &request.user, state, &env).await?;
    }

    Ok(ChainOutcome {
        chain,
        link: links.first().cloned().unwrap_or_default(),
        boxes: installed,
    })
}

/// The chain's own key material.
///
/// The keypair is the chain's trust anchor: the edge signs the chain leg's
/// credential with the secret, and every hop verifies that signature against
/// the public key. One keypair for the whole chain — a hop that carried a
/// different anchor would refuse the leg.
///
/// The mesh id is the EDGE's, read from the edge's own link: the customer's
/// link carries the edge's mesh id, and the chain's legs have to match it
/// byte for byte.
fn chain_keys(edge: &BoxArtefact) -> Result<ChainKeys, String> {
    let mesh_id = edge.mesh_id.trim().to_owned();
    if mesh_id.len() != MESH_ID_HEX_LEN || !mesh_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the edge's link carries no usable mesh id".to_owned());
    }
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    let signing = SigningKey::from_bytes(&seed);
    Ok(ChainKeys {
        operator_pubkey_hex: hex::encode(signing.verifying_key().to_bytes()),
        operator_secret_hex: hex::encode(signing.to_bytes()),
        mesh_id,
    })
}

/// The door the chain leg to this box uses.
///
/// The first TCP door the box published. The leg rides TCP, so a datagram
/// door cannot carry it — and the doors are published in the order the box
/// prefers them, so the first TCP door is the one the box would have used
/// for a client anyway.
fn pick_face(artefact: &BoxArtefact) -> Option<ChainFace> {
    artefact
        .entrances
        .iter()
        .find(|entrance| !is_datagram(&entrance.camouflage))
        .map(face_of)
}

/// The edge's own door.
///
/// The edge takes no chain leg: the customer dials it, and every door the
/// edge published is a door the customer may dial — the link speaks every
/// transport the edge offers. So the edge needs no TCP door of its own, and
/// the first door it published is the one the chain keeps.
fn pick_edge_face(artefact: &BoxArtefact) -> Option<ChainFace> {
    artefact.entrances.first().map(face_of)
}

/// One published door, as the chain keeps it.
fn face_of(entrance: &ArtefactEntrance) -> ChainFace {
    ChainFace {
        addr: entrance.addr.clone(),
        pubkey_hex: entrance.server_pubkey_hex.clone(),
        camouflage: entrance.camouflage.to_string(),
    }
}

/// Whether this mask speaks datagrams rather than a stream.
fn is_datagram(camouflage: &serde_json::Value) -> bool {
    camouflage
        .get("type")
        .and_then(|t| t.as_str())
        .map(|t| UDP_CAMOUFLAGE_TYPES.contains(&t))
        .unwrap_or(false)
}

/// The state directory of one box of the chain.
///
/// Under the default state directory, one directory per box: the client's key
/// for that box, the pin of that box's host key, and that box's link.
fn box_state_dir(request: &ChainRequest) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("exe: {e}"))?;
    let base = exe
        .parent()
        .ok_or("exe dir")?
        .join("standalone")
        .join("boxes");
    std::fs::create_dir_all(&base).map_err(|e| format!("mkdir: {e}"))?;
    let dir = base.join(sanitize(&request.host));
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    Ok(dir)
}

/// A box's address, as a directory name.
///
/// The address is a name and not a path: a name with a slash in it would
/// make a directory somewhere else entirely.
fn sanitize(host: &str) -> String {
    let cleaned: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "box".to_owned()
    } else {
        cleaned
    }
}

/// The chain's own state file — what the client remembers about the chain it
/// built.
pub fn save_chain(dir: &Path, outcome: &ChainOutcome) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
    let json = serde_json::to_string_pretty(&outcome.chain).map_err(|e| format!("chain: {e}"))?;
    std::fs::write(dir.join("chain.json"), json).map_err(|e| format!("save chain: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artefact(mesh_id: &str, entrances: Vec<ArtefactEntrance>) -> BoxArtefact {
        BoxArtefact {
            mesh_id: mesh_id.to_owned(),
            entrances,
        }
    }

    fn entrance(addr: &str, camouflage: serde_json::Value) -> ArtefactEntrance {
        ArtefactEntrance {
            addr: addr.to_owned(),
            server_pubkey_hex: "11".repeat(32),
            camouflage,
        }
    }

    #[test]
    fn a_datagram_door_is_not_a_chain_leg() {
        assert!(is_datagram(&serde_json::json!({"type": "quic"})));
        assert!(is_datagram(&serde_json::json!({"type": "raw_udp"})));
        assert!(!is_datagram(&serde_json::json!({"type": "tls_real"})));
        assert!(!is_datagram(&serde_json::json!({"type": "web_socket"})));
    }

    #[test]
    fn the_first_tcp_door_is_the_one_the_leg_uses() {
        let artefact = artefact(
            &"ab".repeat(32),
            vec![
                entrance("10.0.0.1:443", serde_json::json!({"type": "quic"})),
                entrance(
                    "10.0.0.1:8443",
                    serde_json::json!({"type": "tls_real", "sni": "a.example"}),
                ),
            ],
        );
        let face = pick_face(&artefact).expect("a TCP door");
        assert_eq!(face.addr, "10.0.0.1:8443");
        assert_eq!(face.pubkey_hex, "11".repeat(32));
        assert!(face.camouflage.contains("tls_real"));
    }

    #[test]
    fn a_box_with_only_datagram_doors_has_no_leg() {
        let artefact = artefact(
            &"ab".repeat(32),
            vec![entrance(
                "10.0.0.1:443",
                serde_json::json!({"type": "quic"}),
            )],
        );
        assert!(pick_face(&artefact).is_none());
    }

    #[test]
    fn the_edge_keeps_its_first_door_whatever_it_carries() {
        // The edge takes no chain leg: the customer dials it, and every door
        // it published is a door the customer may dial.
        let artefact = artefact(
            &"ab".repeat(32),
            vec![entrance(
                "10.0.0.1:443",
                serde_json::json!({"type": "quic"}),
            )],
        );
        let face = pick_edge_face(&artefact).expect("a door");
        assert_eq!(face.addr, "10.0.0.1:443");
    }

    #[test]
    fn the_chain_gets_one_keypair_and_the_edges_mesh_id() {
        let edge = artefact(
            &"ab".repeat(32),
            vec![entrance(
                "10.0.0.1:443",
                serde_json::json!({"type": "tls_real"}),
            )],
        );
        let keys = chain_keys(&edge).expect("keys");
        assert_eq!(keys.mesh_id, "ab".repeat(32));
        assert_eq!(keys.operator_pubkey_hex.len(), 64);
        assert_eq!(keys.operator_secret_hex.len(), 64);
        assert!(keys.operator_pubkey_hex.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(keys.operator_secret_hex.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn two_chains_get_two_keypairs() {
        let edge = artefact(
            &"ab".repeat(32),
            vec![entrance(
                "10.0.0.1:443",
                serde_json::json!({"type": "tls_real"}),
            )],
        );
        let first = chain_keys(&edge).expect("keys");
        let second = chain_keys(&edge).expect("keys");
        assert_ne!(first.operator_secret_hex, second.operator_secret_hex);
        assert_ne!(first.operator_pubkey_hex, second.operator_pubkey_hex);
    }

    #[test]
    fn an_edge_without_a_mesh_id_is_refused() {
        let edge = artefact(
            "",
            vec![entrance(
                "10.0.0.1:443",
                serde_json::json!({"type": "tls_real"}),
            )],
        );
        assert!(chain_keys(&edge).is_err());
    }

    #[test]
    fn an_address_becomes_a_name_and_not_a_path() {
        assert_eq!(sanitize("192.0.2.10"), "192.0.2.10");
        assert_eq!(sanitize("10.0.0.1:443"), "10.0.0.1_443");
        assert_eq!(sanitize("../etc"), ".._etc");
        assert_eq!(sanitize(""), "box");
    }
}
