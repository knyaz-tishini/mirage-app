//! A chain of boxes, and the `server.env` each of them is given.
//!
//! # The shape of a chain
//!
//! A chain is a list of boxes. The FIRST box is the EDGE — the box the
//! customer talks to, the one that owns the chain and dials it. The boxes
//! after it are the chain's HOPS: the edge dials the first hop, that hop
//! dials the next one, and the last hop sends the traffic out to the
//! internet.
//!
//! The edge is NOT a hop of its own chain. It stands in front of the chain:
//! the customer's link points at it, and the chain starts behind it.
//!
//! # What every box is told
//!
//! Every box of the chain is told the SAME chain — the same hop list, the
//! same trust anchor, the same mesh id — and the boxes differ in two lines
//! only: a hop knows its place among the hops, and the edge alone keeps the
//! chain's signing secret.
//!
//! ```text
//! MIRAGE_CHAIN_ID=stand-2hop
//! MIRAGE_CHAIN_LENGTH=1
//! MIRAGE_CHAIN_POSITION=0
//! MIRAGE_CHAIN_HOP_0_ADDR=<the hop's address>
//! MIRAGE_CHAIN_HOP_0_PUBKEY=<the hop's wire key>
//! MIRAGE_CHAIN_HOP_0_MASK={"type":"tls_real","sni":"…"}
//! MIRAGE_OPERATOR_PUBKEY_HEX=<the chain's operator public key>
//! MIRAGE_MESH_ID=<the chain's shared mesh id>
//! MIRAGE_CHAIN_OPERATOR_SECRET_HEX=<the chain's operator secret>
//! ```
//!
//! `MIRAGE_CHAIN_POSITION` is on the HOPS only, and the secret is on the
//! EDGE only. `MIRAGE_CHAIN_LENGTH` counts the HOPS — the edge is not one of
//! them — so a chain of two boxes has one hop and says so.
//!
//! # Why every box gets the WHOLE chain
//!
//! The hop list is the same on every box, and that is load-bearing. A box
//! told only about "the hop after me" has never heard of the one in front
//! of it: the exit would have no way to recognise the entry that knocks on
//! its door, and no way to accept its leg. The exit needs the entry's key
//! and address exactly as much as the entry needs the exit's.
//!
//! # The client's own door
//!
//! The client dials the EDGE — the first box of the chain — and the edge
//! dials the chain's first HOP, which is the second box. Everything past
//! that is the chain's own business, and a client that joins the same chain
//! later walks the same road without knowing it.

use serde::{Deserialize, Serialize};

/// What a HOP does in the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HopRole {
    /// The hop takes the chain from the edge and relays onward.
    Entry,
    /// The hop relays onward, and the chain continues past it.
    Middle,
    /// The hop is the end of the chain.
    Exit,
}

impl HopRole {
    /// The role a HOP plays at `position` among a chain of `length` hops.
    ///
    /// The first hop takes the chain from the edge and relays onward, the
    /// last hop ends the chain, and everything between relays onward. A
    /// chain of ONE hop is the exit: it takes the chain from the edge and
    /// sends the traffic out by itself.
    pub fn for_position(position: usize, length: usize) -> Self {
        if length <= 1 {
            HopRole::Exit
        } else if position == 0 {
            HopRole::Entry
        } else if position + 1 == length {
            HopRole::Exit
        } else {
            HopRole::Middle
        }
    }

    /// The role as the env file spells it.
    pub fn as_env(&self) -> &'static str {
        match self {
            HopRole::Entry => "entry",
            HopRole::Middle => "middle",
            HopRole::Exit => "exit",
        }
    }
}

/// Where a box sits in the chain.
///
/// The chain has two kinds of box: the EDGE — the customer-facing one that
/// owns the chain and dials it — and the HOPS, the boxes the edge dials in
/// turn. Only a hop has a [`HopRole`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChainRole {
    /// The edge: the customer-facing box that owns the chain.
    Edge,
    /// A hop of the chain, with the role it plays among the hops.
    Hop(HopRole),
}

impl ChainRole {
    /// The role as the env file spells it.
    pub fn as_env(&self) -> &'static str {
        match self {
            ChainRole::Edge => "edge",
            ChainRole::Hop(role) => role.as_env(),
        }
    }
}

/// The door a customer dials: an address, the key that answers it, and the
/// link it speaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainEntrance {
    /// `host:port` the customer knocks on.
    pub addr: String,
    /// The public key the customer verifies the box by, hex.
    pub server_pubkey_hex: String,
    /// The link the door speaks, as the server publishes it.
    pub protocol: serde_json::Value,
}

/// One door of a box, as the box published it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainFace {    /// `host:port` the box published this door on.
    pub addr: String,
    /// The box's wire public key, hex — the key the chain leg verifies the
    /// box by.
    pub pubkey_hex: String,
    /// The mask the door wears, as the JSON the server reads it back from.
    pub camouflage: String,
}

/// One box of the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainBox {
    /// The box's address, as the operator named it.
    pub host: String,
    /// The login the client uses on the box.
    pub user: String,
    /// The door the box published.
    ///
    /// For a HOP this is the door the chain leg to it uses. For the EDGE it
    /// is the edge's own door — the one the customer's link points at, and
    /// not a door of the chain.
    pub face: ChainFace,
}

/// The key material every box of one chain shares.
///
/// One chain, one trust anchor: the edge signs the chain leg's credential
/// with the secret, and every hop verifies that signature against the
/// public key. A hop that does not know the anchor refuses the leg, so the
/// anchor is written into EVERY box of the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainKeys {
    /// The chain's operator PUBLIC key, hex — the trust anchor.
    pub operator_pubkey_hex: String,
    /// The chain's operator PRIVATE seed, hex — the edge signs with it.
    ///
    /// Only the edge is told this. A hop never signs the chain leg, so a
    /// hop never gets the secret.
    pub operator_secret_hex: String,
    /// The chain's shared mesh id, hex.
    ///
    /// The chain leg's credential carries a mesh id and the receiving hop
    /// compares it byte for byte with its own — so every box of one chain
    /// carries the same value, and that value is the EDGE's: the customer's
    /// own link carries the edge's mesh id.
    pub mesh_id: String,
}

/// A whole chain, as the client keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chain {
    /// The chain's id — the same in every box.
    pub chain_id: String,
    /// The boxes, in the order the traffic walks them: the edge first, then
    /// the hops.
    pub boxes: Vec<ChainBox>,
    /// The key material the whole chain shares.
    pub keys: ChainKeys,
    /// The port every hop of the chain answers its datagram leg on.
    ///
    /// One number for the whole chain: the boxes are different machines, and
    /// the number only has to be free on the box that binds it. It is the
    /// chain's own number — the server's profile table owns it, and the
    /// client reads it out of the box it captures.
    pub chain_udp_port: u16,
    /// The port the packet PLANE is served on.
    ///
    /// One number for the whole chain, and the same number everywhere: the
    /// customer's door at the edge (the relay that carries the customer's
    /// QUIC into the chain) and the plane at the exit (the box the
    /// customer's packets come out of). The customer knocks on the edge's
    /// number; the edge carries the QUIC to the exit's.
    pub plane_port: u16,
    /// The door the CUSTOMER dials.
    ///
    /// It is filled in once the chain is built: the edge's plane port, with
    /// the exit's key. See [`Chain::customer_entrance`].
    pub customer: Option<ChainEntrance>,
}

impl Chain {
    /// The chain's HOPS: the boxes after the edge.
    ///
    /// The edge is not a hop of its own chain — it owns the chain and dials
    /// it — so the hop list starts at the second box.
    pub fn hops(&self) -> &[ChainBox] {
        self.boxes.get(1..).unwrap_or(&[])
    }

    /// How many hops the chain has.
    ///
    /// The edge is not one of them, so a chain of two boxes has one hop.
    pub fn hop_count(&self) -> usize {
        self.hops().len()
    }

    /// The role of the box at `index`.
    ///
    /// The first box is the edge; every box after it is a hop, and the hop's
    /// role is its place among the hops.
    pub fn role_of(&self, index: usize) -> ChainRole {
        match index {
            0 => ChainRole::Edge,
            index => ChainRole::Hop(HopRole::for_position(index - 1, self.hop_count())),
        }
    }

    /// The door the CLIENT dials: the EDGE's own published door.
    ///
    /// The edge stands in front of the chain: the customer's link points at
    /// it, and it dials the chain's first HOP — the second box of the
    /// chain, not this one.
    pub fn entry_face(&self) -> Option<&ChainFace> {
        self.boxes.first().map(|node| &node.face)
    }

    /// The door the CUSTOMER dials: the edge's PLANE, with the exit's key.
    ///
    /// The customer's QUIC does not end at the edge: the edge relays it into
    /// the chain, and the chain carries it to the box that serves the plane.
    /// So the customer's door is the edge's plane port, and the key the
    /// customer verifies is the EXIT's — the box that actually terminates
    /// the customer's QUIC. A key of the edge would be the wrong key: the
    /// edge never sees the handshake.
    pub fn customer_entrance(&self) -> Option<ChainEntrance> {
        let edge = self.boxes.first()?;
        let exit = self.boxes.last()?;
        Some(ChainEntrance {
            addr: format!("{}:{}", host_of(&edge.face.addr), self.plane_port),
            server_pubkey_hex: exit.face.pubkey_hex.clone(),
            protocol: serde_json::json!({"type": "quic", "alpn": ["h3"]}),
        })
    }

    /// Build the `server.env` for the box at `index`.
    ///
    /// `base` is what the box needs for ITSELF — the stack, the protocols,
    /// the ports, the masks — and the chain is appended to it. The base is
    /// written first, so a key the chain also writes would win: the chain
    /// is the part that must not be half-written.
    pub fn server_env(&self, index: usize, base: &str) -> Result<String, ChainError> {
        if index >= self.boxes.len() {
            return Err(ChainError::NoSuchBox { index });
        }
        let hops = self.hops();
        let mut env = String::new();
        if !base.trim().is_empty() {
            env.push_str(base.trim_end());
            env.push('\n');
        }
        // A chain of ONE box is a box that stands alone: there is no hop to
        // walk, so the box is told NOTHING about a chain and exits from
        // itself. Writing a chain of zero hops would only name a chain that
        // cannot exist — and the box would refuse to start.
        if hops.is_empty() {
            return Ok(env);
        }
        if self.keys.operator_pubkey_hex.trim().is_empty() {
            // Without the anchor the hops would refuse every chain leg, and
            // the box would come up looking healthy while carrying nothing.
            return Err(ChainError::NoOperatorKey);
        }
        env.push_str(&format!("MIRAGE_CHAIN_ID={}\n", self.chain_id));
        env.push_str(&format!("MIRAGE_CHAIN_LENGTH={}\n", hops.len()));
        // The chain's own doors: the port every hop answers its datagram leg
        // on, and the port the packet plane is served on. Both are the same
        // on every box — the chain is one configuration, and the ports are
        // part of it. Which box serves the plane and which one relays it
        // follows from the box's place in the chain, not from the file.
        env.push_str(&format!("MIRAGE_CHAIN_UDP_PORT={}\n", self.chain_udp_port));
        env.push_str(&format!("MIRAGE_PLANE_PORT={}\n", self.plane_port));
        env.push_str("MIRAGE_PLANE_ALPN=h3\n");
        // Only a hop has a place among the hops. The edge owns the chain and
        // has no position in it.
        if index > 0 {
            env.push_str(&format!("MIRAGE_CHAIN_POSITION={}\n", index - 1));
        }
        // The hop list is the SAME on every box — the exit has to know the
        // entry's key and address, and the entry has to know the exit's.
        for (hop, node) in hops.iter().enumerate() {
            env.push_str(&format!("MIRAGE_CHAIN_HOP_{hop}_ADDR={}\n", node.face.addr));
            env.push_str(&format!(
                "MIRAGE_CHAIN_HOP_{hop}_PUBKEY={}\n",
                node.face.pubkey_hex
            ));
            env.push_str(&format!(
                "MIRAGE_CHAIN_HOP_{hop}_MASK={}\n",
                one_line(&node.face.camouflage)?
            ));
            // The hop's DATAGRAM door, on the port every hop of the chain
            // answers datagrams on. It is the same number on every hop —
            // they are different boxes, and the number only has to be free on
            // the box that binds it.
            env.push_str(&format!(
                "MIRAGE_CHAIN_HOP_{hop}_UDP_ADDR={}:{}\n",
                host_of(&node.face.addr),
                self.chain_udp_port
            ));
        }
        // The trust anchor and the mesh id: on EVERY box of the chain.
        env.push_str(&format!(
            "MIRAGE_OPERATOR_PUBKEY_HEX={}\n",
            self.keys.operator_pubkey_hex
        ));
        env.push_str(&format!("MIRAGE_MESH_ID={}\n", self.keys.mesh_id));
        // The secret signs the chain leg's credential, and only the edge
        // signs it.
        if index == 0 {
            env.push_str(&format!(
                "MIRAGE_CHAIN_OPERATOR_SECRET_HEX={}\n",
                self.keys.operator_secret_hex
            ));
        } else {
            // This box is a HOP of the chain: it answers the chain's datagram
            // leg on this port. Without it the chain carries TCP only, and a
            // customer's datagram would have to be refused rather than
            // delivered out of the wrong box.
            env.push_str(&format!("MIRAGE_CHAIN_UDP_PORT={}\n", CHAIN_UDP_PORT));
        }
        Ok(env)
    }
}

/// The port every hop of a chain answers its datagram leg on.
///
/// The number belongs to the SERVER's port table — the profile that names
/// the protocols names this one too — and the client reads it out of the box
/// it captures, so the two never drift apart. This is the fallback for a box
/// whose environment does not name it.
///
/// A high port, out of the way of the doors a box publishes for customers
/// (80, 443, 8080, 8443) and of the UDP doors the node binds itself (QUIC
/// and friends). TCP and UDP have separate port spaces, so this number may
/// coincide with a TCP face's — but not with another UDP bind on the same
/// box, and the box refuses to start rather than shadowing one.
pub const CHAIN_UDP_PORT: u16 = 51822;

/// The port the packet PLANE is served on.
///
/// The customer's door at the edge and the plane at the exit are the SAME
/// number: the customer knocks on the edge's, and the edge carries the
/// customer's QUIC to the exit's. One number for the whole chain.
pub const PLANE_PORT: u16 = 51823;

/// The host part of a `host:port` address.
///
/// The chain's datagram door is named by its host and the chain's own UDP
/// port — not by the port the box published for its streams.
fn host_of(addr: &str) -> &str {
    match addr.rsplit_once(':') {
        // A bracketed IPv6 address keeps its brackets: "[::1]:443" → "[::1]".
        Some((host, _)) if host.ends_with(']') || !host.contains(':') => host,
        _ => addr,
    }
}

/// Everything that can go wrong while building a chain.
#[derive(Debug, PartialEq, Eq)]
pub enum ChainError {
    /// The chain was asked for a box it does not have.
    NoSuchBox {
        /// The position that was asked for.
        index: usize,
    },

    /// The chain carries no trust anchor, so no hop would accept its leg.
    NoOperatorKey,

    /// A mask carries a line break, and a line break in an env value would
    /// silently cut the value in two.
    MaskNotOneLine,
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainError::NoSuchBox { index } => {
                write!(f, "chain: there is no box at position {index}")
            }
            ChainError::NoOperatorKey => {
                write!(f, "chain: the chain carries no operator key")
            }
            ChainError::MaskNotOneLine => write!(f, "chain: the mask of a hop is not one line"),
        }
    }
}

impl std::error::Error for ChainError {}

impl From<ChainError> for String {
    fn from(value: ChainError) -> Self {
        value.to_string()
    }
}

/// The mask as a single env line.
///
/// An env value ends at the line break: a mask written over two lines would
/// be read as a mask and as a nameless key, and the box would wear no mask
/// at all while the operator believed it wore one.
fn one_line(mask: &str) -> Result<String, ChainError> {
    let trimmed = mask.trim();
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(ChainError::MaskNotOneLine);
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(addr: &str) -> ChainFace {
        ChainFace {
            addr: addr.to_owned(),
            pubkey_hex: "22".repeat(32),
            camouflage: r#"{"type":"tls_real","sni":"example.com"}"#.to_owned(),
        }
    }

    fn keys() -> ChainKeys {
        ChainKeys {
            operator_pubkey_hex: "ab".repeat(32),
            operator_secret_hex: "cd".repeat(32),
            mesh_id: "ef".repeat(32),
        }
    }

    fn chain() -> Chain {
        Chain {
            chain_id: "stand-2hop".to_owned(),
            boxes: vec![
                ChainBox {
                    host: "192.0.2.10".to_owned(),
                    user: "root".to_owned(),
                    face: face("192.0.2.10:443"),
                },
                ChainBox {
                    host: "192.0.2.20".to_owned(),
                    user: "root".to_owned(),
                    face: face("192.0.2.20:443"),
                },
            ],
            keys: keys(),
        }
    }

    #[test]
    fn the_first_box_is_the_edge_and_the_last_hop_is_the_exit() {
        let chain = chain();
        assert_eq!(chain.role_of(0), ChainRole::Edge);
        assert_eq!(chain.role_of(1), ChainRole::Hop(HopRole::Exit));
    }

    #[test]
    fn a_chain_of_one_hop_is_the_exit() {
        assert_eq!(HopRole::for_position(0, 1), HopRole::Exit);
    }

    #[test]
    fn a_chain_of_one_box_is_the_edge_and_has_no_hops() {
        let chain = Chain {
            chain_id: "solo".to_owned(),
            boxes: vec![ChainBox {
                host: "10.0.0.1".to_owned(),
                user: "root".to_owned(),
                face: face("10.0.0.1:443"),
            }],
            keys: keys(),
        };
        assert_eq!(chain.role_of(0), ChainRole::Edge);
        assert_eq!(chain.hop_count(), 0);
    }

    #[test]
    fn a_chain_of_one_box_writes_no_chain_at_all() {
        // A box that stands alone has no hop to walk: it is told NOTHING
        // about a chain and exits from itself. A chain of zero hops would
        // name a chain that cannot exist, and the box would refuse to start.
        let chain = Chain {
            chain_id: "solo".to_owned(),
            boxes: vec![ChainBox {
                host: "10.0.0.1".to_owned(),
                user: "root".to_owned(),
                face: face("10.0.0.1:443"),
            }],
            keys: keys(),
        };
        let env = chain.server_env(0, "STACK=auto\n").expect("env");
        assert_eq!(env, "STACK=auto\n");
        assert!(
            !env.contains("MIRAGE_CHAIN"),
            "a box that stands alone is told no chain: {env}"
        );
        assert!(
            !env.contains("MIRAGE_MESH_ID"),
            "a box that stands alone keeps its own mesh id: {env}"
        );
    }

    #[test]
    fn a_middle_hop_is_a_middle_hop() {
        let chain = Chain {
            chain_id: "four".to_owned(),
            boxes: vec![
                ChainBox {
                    host: "10.0.0.1".to_owned(),
                    user: "root".to_owned(),
                    face: face("10.0.0.1:443"),
                },
                ChainBox {
                    host: "10.0.0.2".to_owned(),
                    user: "root".to_owned(),
                    face: face("10.0.0.2:443"),
                },
                ChainBox {
                    host: "10.0.0.3".to_owned(),
                    user: "root".to_owned(),
                    face: face("10.0.0.3:443"),
                },
                ChainBox {
                    host: "10.0.0.4".to_owned(),
                    user: "root".to_owned(),
                    face: face("10.0.0.4:443"),
                },
            ],
            keys: keys(),
        };
        assert_eq!(chain.role_of(0), ChainRole::Edge);
        assert_eq!(chain.role_of(1), ChainRole::Hop(HopRole::Entry));
        assert_eq!(chain.role_of(2), ChainRole::Hop(HopRole::Middle));
        assert_eq!(chain.role_of(3), ChainRole::Hop(HopRole::Exit));
    }

    #[test]
    fn the_edge_has_no_position_and_the_hops_have_theirs() {
        let chain = chain();
        let edge = chain.server_env(0, "STACK=auto\n").expect("env");
        let hop = chain.server_env(1, "STACK=auto\n").expect("env");

        assert!(!edge.contains("MIRAGE_CHAIN_POSITION"), "the edge has no position");
        assert!(hop.contains("MIRAGE_CHAIN_POSITION=0"), "the only hop is at 0");
    }

    #[test]
    fn the_hop_count_is_the_number_of_hops() {
        let chain = chain();
        let edge = chain.server_env(0, "").expect("env");
        let hop = chain.server_env(1, "").expect("env");
        assert!(edge.contains("MIRAGE_CHAIN_LENGTH=1"));
        assert!(hop.contains("MIRAGE_CHAIN_LENGTH=1"));
    }

    #[test]
    fn every_box_is_told_the_same_hops() {
        let chain = chain();
        let edge = chain.server_env(0, "").expect("env");
        let hop = chain.server_env(1, "").expect("env");

        // The edge is not a hop of its own chain: only the second box is.
        for env in [&edge, &hop] {
            assert!(env.contains("MIRAGE_CHAIN_HOP_0_ADDR=192.0.2.20:443"));
            assert!(env.contains(&format!("MIRAGE_CHAIN_HOP_0_PUBKEY={}", "22".repeat(32))));
            assert!(env.contains("MIRAGE_CHAIN_HOP_0_MASK="));
            assert!(!env.contains("MIRAGE_CHAIN_HOP_1_"), "the edge is not a hop");
        }
    }

    #[test]
    fn the_shared_keys_are_on_every_box_and_the_secret_only_on_the_edge() {
        let chain = chain();
        let edge = chain.server_env(0, "").expect("env");
        let hop = chain.server_env(1, "").expect("env");

        for env in [&edge, &hop] {
            assert!(env.contains(&format!("MIRAGE_OPERATOR_PUBKEY_HEX={}", "ab".repeat(32))));
            assert!(env.contains(&format!("MIRAGE_MESH_ID={}", "ef".repeat(32))));
        }
        assert!(edge.contains(&format!(
            "MIRAGE_CHAIN_OPERATOR_SECRET_HEX={}",
            "cd".repeat(32)
        )));
        assert!(!hop.contains("MIRAGE_CHAIN_OPERATOR_SECRET_HEX"));
    }

    #[test]
    fn a_chain_without_an_anchor_is_refused() {
        let mut chain = chain();
        chain.keys.operator_pubkey_hex = String::new();
        assert_eq!(
            chain.server_env(0, "").expect_err("no anchor"),
            ChainError::NoOperatorKey
        );
    }

    #[test]
    fn the_base_env_survives() {
        let chain = chain();
        let env = chain
            .server_env(0, "STACK=auto\nPROTOCOLS=cloak\nCLOAK_PORT=443\n")
            .expect("env");
        assert!(env.starts_with("STACK=auto\nPROTOCOLS=cloak\nCLOAK_PORT=443\n"));
    }

    #[test]
    fn a_box_the_chain_does_not_have_is_refused() {
        let chain = chain();
        assert_eq!(
            chain.server_env(2, "").expect_err("no such box"),
            ChainError::NoSuchBox { index: 2 }
        );
    }

    #[test]
    fn a_mask_over_two_lines_is_refused() {
        let mut chain = chain();
        chain.boxes[1].face.camouflage = "{\n\"type\":\"tls_real\"\n}".to_owned();
        assert_eq!(
            chain.server_env(0, "").expect_err("two lines"),
            ChainError::MaskNotOneLine
        );
    }
}
