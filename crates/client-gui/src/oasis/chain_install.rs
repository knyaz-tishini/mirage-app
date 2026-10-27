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
//! # The keys
//!
//! Every box gets its own state directory: its own client key, its own
//! host-key pin, its own link. A chain of two boxes that shared one
//! directory would have the second box's install overwrite the first box's
//! pin — and the first box's pin is the thing that catches a swapped-out
//! machine.

use std::path::{Path, PathBuf};

use super::chain::{Chain, ChainBox, ChainFace, HopRole};
use super::provision::{self, InstallBus, Job, Outcome};

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
    "hysteria2",
    "raw_udp",
];

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
    /// What the box does in the chain.
    pub role: HopRole,
    /// How many doors the box published.
    pub faces: usize,
    /// The door the chain leg to this box uses.
    pub face: ChainFace,
}

/// What the chain turned out to be.
#[derive(Debug, Clone)]
pub struct ChainOutcome {
    /// The chain, as the client built it.
    pub chain: Chain,
    /// The link the client dials — the entry box's own.
    ///
    /// A client that joins the chain walks the same road as everybody else,
    /// so it starts where the chain starts.
    pub link: String,
    /// Every box, in chain order.
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

    // Step two: what each box published becomes the chain's hops.
    let mut chain_boxes = Vec::with_capacity(boxes.len());
    let mut installed = Vec::with_capacity(boxes.len());
    for (index, (request, link)) in boxes.iter().zip(links.iter()).enumerate() {
        let artefact: BoxArtefact = serde_json::from_str(link)
            .map_err(|e| format!("{}: the box's link is not readable: {e}", request.host))?;
        let face = pick_face(&artefact).ok_or_else(|| {
            format!(
                "{}: the box published no TCP door for the chain leg to use",
                request.host
            )
        })?;
        let role = HopRole::for_position(index, boxes.len());
        chain_boxes.push(ChainBox {
            host: request.host.clone(),
            user: request.user.clone(),
            face: face.clone(),
        });
        installed.push(InstalledBox {
            host: request.host.clone(),
            role,
            faces: artefact.entrances.len(),
            face,
        });
    }

    let chain = Chain {
        chain_id: chain_id.to_owned(),
        boxes: chain_boxes,
    };

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
        .map(|entrance| ChainFace {
            addr: entrance.addr.clone(),
            pubkey_hex: entrance.server_pubkey_hex.clone(),
            camouflage: entrance
                .camouflage
                .get("type")
                .and_then(|t| t.as_str())
                .map(|t| entrance.camouflage.to_string())
                .unwrap_or_else(|| entrance.camouflage.to_string()),
        })
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

    #[test]
    fn a_datagram_door_is_not_a_chain_leg() {
        assert!(is_datagram(&serde_json::json!({"type": "quic"})));
        assert!(is_datagram(&serde_json::json!({"type": "raw_udp"})));
        assert!(!is_datagram(&serde_json::json!({"type": "tls_real"})));
        assert!(!is_datagram(&serde_json::json!({"type": "web_socket"})));
    }

    #[test]
    fn the_first_tcp_door_is_the_one_the_leg_uses() {
        let artefact = BoxArtefact {
            entrances: vec![
                ArtefactEntrance {
                    addr: "10.0.0.1:443".to_owned(),
                    server_pubkey_hex: "11".repeat(32),
                    camouflage: serde_json::json!({"type": "quic"}),
                },
                ArtefactEntrance {
                    addr: "10.0.0.1:8443".to_owned(),
                    server_pubkey_hex: "11".repeat(32),
                    camouflage: serde_json::json!({"type": "tls_real", "sni": "a.example"}),
                },
            ],
        };
        let face = pick_face(&artefact).expect("a TCP door");
        assert_eq!(face.addr, "10.0.0.1:8443");
        assert_eq!(face.pubkey_hex, "11".repeat(32));
        assert!(face.camouflage.contains("tls_real"));
    }

    #[test]
    fn a_box_with_only_datagram_doors_has_no_leg() {
        let artefact = BoxArtefact {
            entrances: vec![ArtefactEntrance {
                addr: "10.0.0.1:443".to_owned(),
                server_pubkey_hex: "11".repeat(32),
                camouflage: serde_json::json!({"type": "quic"}),
            }],
        };
        assert!(pick_face(&artefact).is_none());
    }

    #[test]
    fn an_address_becomes_a_name_and_not_a_path() {
        assert_eq!(sanitize("144.172.115.106"), "144.172.115.106");
        assert_eq!(sanitize("10.0.0.1:443"), "10.0.0.1_443");
        assert_eq!(sanitize("../etc"), ".._etc");
        assert_eq!(sanitize(""), "box");
    }
}
