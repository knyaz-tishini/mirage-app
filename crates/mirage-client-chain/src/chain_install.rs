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
//!
//! # The two shapes of a published door
//!
//! A box publishes its doors, and the shape of what it published depends on
//! who published it. The standalone box names the whole LINK of every door —
//! `{"type":"mirage","carrier":"tcp","camouflage":{"type":"none"}}` — while
//! an older artefact names the MASK the door wears —
//! `{"type":"tls_real","sni":"…"}`.
//!
//! The chain needs the mask either way, and only the mask: the mask is what
//! the chain leg wears, and it is what the server reads back out of its own
//! env. A door that published its link is reduced to its mask here — the
//! same reduction the server makes for itself.

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
///
/// The list is the mask's own `type`, so it covers both shapes of a
/// published door: the mask a box published, and the mask a published link
/// is reduced to.
const UDP_CAMOUFLAGE_TYPES: &[&str] = &[
    "quic",
    "http3",
    "web_transport",
    "wire_guard",
    "raw_udp",
    // A DNS tunnel and an ICMP tunnel are datagrams too — the server's own
    // `Protocol::is_udp` counts them as doors the perimeter does not bind.
    "dns_tunnel",
    "icmp",
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
///
/// Two shapes are read, because two publishers exist. The standalone box
/// names the whole LINK of every door (`protocol`), and an older artefact
/// names the MASK the door wears (`camouflage`). The chain needs the mask
/// either way — see [`door_mask`].
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
    #[serde(default)]
    camouflage: Option<serde_json::Value>,
    /// The link the door speaks, as the standalone box published it.
    ///
    /// The camouflage is INSIDE it — a `mirage` link carries its own
    /// camouflage — so it is reduced to the mask the server reads back.
    #[serde(default)]
    protocol: Option<serde_json::Value>,
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
            profile: None,
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

    let (chain, installed) = chain_from_links(chain_id, boxes, &links)?;

    // The chain goes into every box, and every box comes back up
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

/// Wire boxes that are already up into one chain.
///
/// Nothing is installed. Each box is read with the key the window already
/// planted: its published link, and the `server.env` it is running. The
/// chain is appended to that env and the container is brought back up with
/// it. The first box is the edge. The last box is the exit.
pub async fn assemble_chain(
    chain_id: &str,
    boxes: &[ChainRequest],
    bus: &InstallBus,
) -> Result<ChainOutcome, String> {
    if boxes.len() < 2 {
        return Err("a chain needs two boxes".to_owned());
    }
    let state = provision::client_state_dir()?;
    let mut links = Vec::with_capacity(boxes.len());
    let mut bases = Vec::with_capacity(boxes.len());
    for request in boxes {
        provision::set_text(bus, &request.host);
        let link = provision::published_link(&request.host, request.port, &request.user)
            .await
            .map_err(|err| format!("{}: {err}", request.host))?;
        let base = provision::read_remote_env(&request.host, request.port, &request.user)
            .await
            .map_err(|err| format!("{}: {err}", request.host))?;
        links.push(link);
        bases.push(base);
    }
    let (chain, installed) = chain_from_links(chain_id, boxes, &links)?;
    for (index, request) in boxes.iter().enumerate() {
        let chain_only = chain.server_env(index, "").map_err(|err| err.to_string())?;
        let env = provision::merge_chain_env(&bases[index], &chain_only);
        provision::reconfigure(&request.host, request.port, &request.user, &state, &env)
            .await
            .map_err(|err| format!("{}: {err}", request.host))?;
    }
    let outcome = ChainOutcome {
        chain,
        link: links.first().cloned().unwrap_or_default(),
        boxes: installed,
    };
    let _ = save_chain(&state, &outcome);
    Ok(outcome)
}

/// What the published links make, before any box is told.
fn chain_from_links(
    chain_id: &str,
    boxes: &[ChainRequest],
    links: &[String],
) -> Result<(Chain, Vec<InstalledBox>), String> {
    if boxes.len() != links.len() || boxes.is_empty() {
        return Err("a chain needs a link from every box".to_owned());
    }
    let mut artefacts = Vec::with_capacity(boxes.len());
    for (request, link) in boxes.iter().zip(links.iter()) {
        let artefact: BoxArtefact = serde_json::from_str(link)
            .map_err(|e| format!("{}: the box's link is not readable: {e}", request.host))?;
        artefacts.push(artefact);
    }
    let keys = chain_keys(&artefacts[0])?;
    let mut chain_boxes = Vec::with_capacity(boxes.len());
    for (index, (request, artefact)) in boxes.iter().zip(artefacts.iter()).enumerate() {
        let face = if index == 0 {
            pick_edge_face(artefact)
        } else {
            pick_face(artefact)
        };
        let face = face
            .map_err(|err| format!("{}: {err}", request.host))?
            .ok_or_else(|| {
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
    let installed = chain
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
    Ok((chain, installed))
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
fn pick_face(artefact: &BoxArtefact) -> Result<Option<ChainFace>, String> {
    for entrance in &artefact.entrances {
        let mask = door_mask(entrance)?;
        if !mask.datagram {
            return Ok(Some(face_of(entrance, &mask)));
        }
    }
    Ok(None)
}

/// The edge's own door.
///
/// The edge takes no chain leg: the customer dials it, and every door the
/// edge published is a door the customer may dial — the link speaks every
/// transport the edge offers. So the edge needs no TCP door of its own, and
/// the first door it published is the one the chain keeps.
fn pick_edge_face(artefact: &BoxArtefact) -> Result<Option<ChainFace>, String> {
    let Some(entrance) = artefact.entrances.first() else {
        return Ok(None);
    };
    let mask = door_mask(entrance)?;
    Ok(Some(face_of(entrance, &mask)))
}

/// One published door, as the chain keeps it.
fn face_of(entrance: &ArtefactEntrance, mask: &DoorMask) -> ChainFace {
    ChainFace {
        addr: entrance.addr.clone(),
        pubkey_hex: entrance.server_pubkey_hex.clone(),
        camouflage: mask.json.to_string(),
    }
}

/// The mask one published door wears.
///
/// The chain writes the mask into the next box's env as
/// `MIRAGE_CHAIN_HOP_<n>_MASK`, and the server reads that value back as a
/// `TransportCamouflage` — a MASK, not a whole link. So a door that
/// published its `protocol` has to be reduced to the mask it carries.
///
/// The reduction mirrors the server's own: `Protocol::to_mask` in
/// `mirage-core` turns a link into the mask the server wears for it, and the
/// chain has to name the SAME mask — a leg that arrives wearing another one
/// is a leg the box does not answer.
///
/// A door that published its `camouflage` already IS the mask. It is taken
/// as it stands, byte for byte.
fn door_mask(entrance: &ArtefactEntrance) -> Result<DoorMask, String> {
    if let Some(protocol) = &entrance.protocol {
        let json = mask_of_protocol(protocol, port_of(&entrance.addr).unwrap_or_default())?;
        return Ok(DoorMask {
            datagram: is_datagram(&json),
            json,
        });
    }
    if let Some(camouflage) = &entrance.camouflage {
        return Ok(DoorMask {
            datagram: is_datagram(camouflage),
            json: camouflage.clone(),
        });
    }
    Err(format!(
        "the door {} carries neither `protocol` nor `camouflage` — the chain cannot tell what mask it wears",
        entrance.addr
    ))
}

/// The mask one published door wears.
#[derive(Debug)]
struct DoorMask {
    /// The mask as JSON — the shape the server reads back out of its env.
    json: serde_json::Value,
    /// Whether the door speaks datagrams rather than a stream.
    datagram: bool,
}

/// The mask a link wears, in the shape the server reads back.
///
/// The links that are not `mirage` are masks themselves under another name:
/// a `tls` link wears `tls_real`, a `dns` link wears `dns_tunnel`, and so
/// on. The datagram masks bind an address of their own — the door's own
/// port, which the door's address carries.
///
/// A link the mesh does not wear as a mask at all — wireguard, amneziawg,
/// shadowsocks — has no mask to name. It falls back to the plain mask, the
/// raw stream the server's own `TransportCamouflage::None` is.
fn mask_of_protocol(
    protocol: &serde_json::Value,
    port: u16,
) -> Result<serde_json::Value, String> {
    let kind = protocol
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    match kind {
        "mirage" => mask_of_mirage(protocol, port),
        "tls" => Ok(renamed(protocol, "tls_real")),
        "websocket" | "web_socket" => Ok(renamed(protocol, "web_socket")),
        "ssh" => Ok(renamed(protocol, "ssh")),
        "grpc" => Ok(renamed(protocol, "grpc")),
        "dns" => Ok(renamed(protocol, "dns_tunnel")),
        "icmp" => Ok(renamed(protocol, "icmp")),
        "quic" => Ok(with_bind(renamed(protocol, "quic"), port)),
        "http3" => Ok(with_bind(renamed(protocol, "http3"), port)),
        "web_transport" => Ok(with_bind(renamed(protocol, "web_transport"), port)),
        "wireguard" | "wire_guard" | "amneziawg" | "amnezia_wg" | "shadowsocks" => {
            Ok(serde_json::json!({ "type": "none" }))
        }
        other => Err(format!(
            "the link speaks `{other}`, which is not a mask the server can wear"
        )),
    }
}

/// The mask a `mirage` link wears.
///
/// The camouflage is inside the link, and the carrier decides how the mask
/// is built: over TCP the link IS the mask, over UDP it is a datagram mask
/// that binds the door's own port.
fn mask_of_mirage(
    protocol: &serde_json::Value,
    port: u16,
) -> Result<serde_json::Value, String> {
    let carrier = protocol
        .get("carrier")
        .and_then(|c| c.as_str())
        .unwrap_or("tcp");
    let camouflage = protocol
        .get("camouflage")
        .ok_or_else(|| "the mirage link carries no camouflage".to_owned())?;
    let kind = camouflage
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    match (carrier, kind) {
        ("tcp", "none") => Ok(serde_json::json!({ "type": "none" })),
        ("udp", "none") => Ok(with_bind(serde_json::json!({ "type": "raw_udp" }), port)),
        ("tcp", "reality") => Ok(renamed(camouflage, "reality")),
        ("tcp", "xhttp") => Ok(renamed(camouflage, "xhttp")),
        ("udp", "junk") => Ok(with_bind(renamed(camouflage, "wire_guard"), port)),
        ("tcp", "junk") => Err("junk is a udp camouflage of mirage".to_owned()),
        ("udp", "reality") => Err("reality is a tcp camouflage of mirage".to_owned()),
        ("udp", "xhttp") => Err("xhttp is a tcp camouflage of mirage".to_owned()),
        (_, other) => Err(format!("`{other}` is not a camouflage this chain knows")),
    }
}

/// A body under another name: the same fields, a different `type`.
///
/// The mask and the link it came from share their field names; only the
/// `type` differs.
fn renamed(body: &serde_json::Value, name: &str) -> serde_json::Value {
    let mut mask = body.clone();
    if let Some(fields) = mask.as_object_mut() {
        fields.insert(
            "type".to_owned(),
            serde_json::Value::String(name.to_owned()),
        );
    }
    mask
}

/// A mask with the address it binds.
///
/// The datagram masks bind an address of their own, and the server binds
/// them to the door's own port — the port the door's address carries.
fn with_bind(mut mask: serde_json::Value, port: u16) -> serde_json::Value {
    if let Some(fields) = mask.as_object_mut() {
        fields.insert(
            "bind_addr".to_owned(),
            serde_json::Value::String(format!("0.0.0.0:{port}")),
        );
    }
    mask
}

/// The port in a `host:port` address.
fn port_of(addr: &str) -> Option<u16> {
    addr.rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
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
    fn two_published_boxes_make_a_relay_then_a_terminator() {
        let mesh = "ab".repeat(32);
        let link = |addr: &str| {
            format!(
                r#"{{"mesh_id":"{mesh}","entrances":[{{"addr":"{addr}","server_pubkey_hex":"{}","camouflage":{{"type":"tls_real"}}}}]}}"#,
                "11".repeat(32)
            )
        };
        let boxes = vec![
            ChainRequest {
                host: "a.example".into(),
                port: 22,
                user: "root".into(),
                password: String::new(),
            },
            ChainRequest {
                host: "b.example".into(),
                port: 22,
                user: "root".into(),
                password: String::new(),
            },
        ];
        let links = vec![link("a.example:443"), link("b.example:443")];
        let (chain, installed) = chain_from_links("gui", &boxes, &links).expect("chain");
        assert_eq!(chain.role_of(0).as_env(), "edge");
        assert_eq!(chain.role_of(1).as_env(), "exit");
        assert_eq!(installed[0].role.as_env(), "edge");
        assert_eq!(installed[1].role.as_env(), "exit");
        let edge = chain.server_env(0, "").expect("edge env");
        let hop = chain.server_env(1, "").expect("hop env");
        assert!(edge.contains("MIRAGE_CHAIN_OPERATOR_SECRET_HEX="));
        assert!(!hop.contains("OPERATOR_SECRET"));
        assert!(hop.contains("MIRAGE_CHAIN_POSITION=0"));
    }

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
            camouflage: Some(camouflage),
            protocol: None,
        }
    }

    /// A door as the STANDALONE box publishes it: the whole link, with the
    /// camouflage inside it.
    fn protocol_entrance(addr: &str, protocol: serde_json::Value) -> ArtefactEntrance {
        ArtefactEntrance {
            addr: addr.to_owned(),
            server_pubkey_hex: "11".repeat(32),
            camouflage: None,
            protocol: Some(protocol),
        }
    }

    /// The mask a door wears, as the chain keeps it.
    fn mask_of(entrance: &ArtefactEntrance) -> serde_json::Value {
        let mask = door_mask(entrance).expect("a mask");
        serde_json::from_str(&mask.json.to_string()).expect("the mask is JSON")
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
        let face = pick_face(&artefact)
            .expect("a readable door")
            .expect("a TCP door");
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
        assert!(matches!(pick_face(&artefact), Ok(None)));
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
        let face = pick_edge_face(&artefact)
            .expect("a readable door")
            .expect("a door");
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

    // ── the two shapes of a published door ─────────────────────────────────

    #[test]
    fn a_standalone_link_and_a_legacy_link_are_the_same_door() {
        // The same door, published the two ways it is published: the
        // standalone box names the whole LINK, the older artefact names the
        // MASK. The chain needs the mask, so both must give the same one.
        let standalone = protocol_entrance(
            "10.0.0.1:443",
            serde_json::json!({"type": "mirage", "carrier": "tcp", "camouflage": {"type": "none"}}),
        );
        let legacy = entrance("10.0.0.1:443", serde_json::json!({"type": "none"}));
        assert_eq!(mask_of(&standalone), serde_json::json!({"type": "none"}));
        assert_eq!(mask_of(&standalone), mask_of(&legacy));

        let from_standalone = pick_face(&artefact(&"ab".repeat(32), vec![standalone]))
            .expect("a readable door")
            .expect("a TCP door");
        let from_legacy = pick_face(&artefact(&"ab".repeat(32), vec![legacy]))
            .expect("a readable door")
            .expect("a TCP door");
        assert_eq!(from_standalone.camouflage, from_legacy.camouflage);
    }

    #[test]
    fn a_standalone_link_becomes_a_mask_the_server_can_wear() {
        // The stock link of a `m1r4g3-server:1` box: a mirage link over TCP
        // with no disguise. The mask the server reads back out of its env is
        // the plain one — `TransportCamouflage::None`.
        let entrance = protocol_entrance(
            "10.0.0.1:443",
            serde_json::json!({"type": "mirage", "carrier": "tcp", "camouflage": {"type": "none"}}),
        );
        assert_eq!(mask_of(&entrance), serde_json::json!({"type": "none"}));
        assert!(!door_mask(&entrance).expect("a mask").datagram);
    }

    #[test]
    fn a_standalone_link_over_udp_is_a_datagram_door() {
        // The same link over UDP binds a datagram mask, and a datagram door
        // cannot carry the chain's TCP leg.
        let entrance = protocol_entrance(
            "10.0.0.1:443",
            serde_json::json!({"type": "mirage", "carrier": "udp", "camouflage": {"type": "none"}}),
        );
        assert_eq!(
            mask_of(&entrance),
            serde_json::json!({"type": "raw_udp", "bind_addr": "0.0.0.0:443"})
        );
        assert!(door_mask(&entrance).expect("a mask").datagram);
    }

    #[test]
    fn a_standalone_link_carries_its_own_disguise() {
        // A disguise inside the link: the mask is the disguise, over TCP.
        let entrance = protocol_entrance(
            "10.0.0.1:443",
            serde_json::json!({
                "type": "mirage",
                "carrier": "tcp",
                "camouflage": {
                    "type": "reality",
                    "short_id": "ab12",
                    "server_name": "a.example",
                    "max_time_diff": 0,
                    "fallback_dest": null
                }
            }),
        );
        assert_eq!(
            mask_of(&entrance),
            serde_json::json!({
                "type": "reality",
                "short_id": "ab12",
                "server_name": "a.example",
                "max_time_diff": 0,
                "fallback_dest": null
            })
        );
    }

    #[test]
    fn a_link_that_is_not_a_mirage_carries_no_disguise_of_its_own() {
        // A QUIC link is a mask under another name — and a datagram one.
        let entrance = protocol_entrance(
            "10.0.0.1:443",
            serde_json::json!({"type": "quic", "alpn": ["h3"]}),
        );
        assert_eq!(
            mask_of(&entrance),
            serde_json::json!({"type": "quic", "alpn": ["h3"], "bind_addr": "0.0.0.0:443"})
        );
        assert!(door_mask(&entrance).expect("a mask").datagram);
    }

    #[test]
    fn a_door_with_neither_field_is_refused() {
        let entrance = ArtefactEntrance {
            addr: "10.0.0.1:443".to_owned(),
            server_pubkey_hex: "11".repeat(32),
            camouflage: None,
            protocol: None,
        };
        let err = door_mask(&entrance).expect_err("no mask at all");
        assert!(err.contains("camouflage"), "{err}");
        assert!(err.contains("protocol"), "{err}");
        assert!(err.contains("10.0.0.1:443"), "{err}");
    }

    #[test]
    fn a_standalone_link_chains_a_real_box() {
        // The whole road: two boxes that published the stock standalone link
        // are chained, and the mask the chain writes into the env is the one
        // the server reads back.
        let mesh = "ab".repeat(32);
        let link = |addr: &str| {
            format!(
                r#"{{"mesh_id":"{mesh}","entrances":[{{"addr":"{addr}","server_pubkey_hex":"{}","protocol":{{"type":"mirage","carrier":"tcp","camouflage":{{"type":"none"}}}}}}]}}"#,
                "11".repeat(32)
            )
        };
        let boxes = vec![
            ChainRequest {
                host: "a.example".into(),
                port: 22,
                user: "root".into(),
                password: String::new(),
            },
            ChainRequest {
                host: "b.example".into(),
                port: 22,
                user: "root".into(),
                password: String::new(),
            },
        ];
        let links = vec![link("a.example:443"), link("b.example:443")];
        let (chain, _) = chain_from_links("gui", &boxes, &links).expect("chain");
        let edge = chain.server_env(0, "").expect("edge env");
        assert!(
            edge.contains(r#"MIRAGE_CHAIN_HOP_0_MASK={"type":"none"}"#),
            "{edge}"
        );
    }

    #[test]
    fn a_door_the_chain_cannot_read_names_the_host_and_the_field() {
        let mesh = "ab".repeat(32);
        let link = format!(
            r#"{{"mesh_id":"{mesh}","entrances":[{{"addr":"b.example:443","server_pubkey_hex":"{}"}}]}}"#,
            "11".repeat(32)
        );
        let boxes = vec![
            ChainRequest {
                host: "a.example".into(),
                port: 22,
                user: "root".into(),
                password: String::new(),
            },
            ChainRequest {
                host: "b.example".into(),
                port: 22,
                user: "root".into(),
                password: String::new(),
            },
        ];
        let links = vec![link.clone(), link];
        let err = chain_from_links("gui", &boxes, &links).expect_err("no mask");
        assert!(err.contains("b.example"), "{err}");
        assert!(err.contains("camouflage"), "{err}");
        assert!(err.contains("protocol"), "{err}");
    }
}
