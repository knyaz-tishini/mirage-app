//! A chain of boxes, and the `server.env` each of them is given.
//!
//! # The shape of a chain
//!
//! A chain is a list of boxes, in the order the client's traffic walks it:
//! the first box receives the client, the last one sends it out to the
//! internet, and everything between relays onward. One box is a chain of
//! one — the exit of its own one-hop chain.
//!
//! # Why every box gets the WHOLE chain
//!
//! The client writes the same chain description to every box in it, and each
//! box reads the same list — differing only in the box's own position:
//!
//! ```text
//! MIRAGE_CHAIN_ID=stand-2hop
//! MIRAGE_CHAIN_POSITION=1
//! MIRAGE_CHAIN_LENGTH=2
//! MIRAGE_CHAIN_HOP_0_ADDR=<the entry box's address>
//! MIRAGE_CHAIN_HOP_0_PUBKEY=<the entry box's wire key>
//! MIRAGE_CHAIN_HOP_0_MASK={"type":"tls_real","sni":"…"}
//! MIRAGE_CHAIN_HOP_1_ADDR=<the exit box's address>
//! MIRAGE_CHAIN_HOP_1_PUBKEY=<the exit box's wire key>
//! MIRAGE_CHAIN_HOP_1_MASK={"type":"tls_real","sni":"…"}
//! ```
//!
//! This is not decoration. A box told only about "the hop after me" has
//! never heard of the one in front of it: the exit box would have no way to
//! recognise the entry box that knocks on its door, and no way to accept its
//! leg. The exit box needs the entry box's key and address exactly as much
//! as the entry box needs the exit box's.
//!
//! The client dials the FIRST box of the chain — the entry. Everything past
//! it is the chain's own business, and a client that joins the same chain
//! later walks the same road without knowing it.

use serde::{Deserialize, Serialize};

/// What a box does in the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HopRole {
    /// The box receives the client and relays onward.
    Entry,
    /// The box relays onward, and the chain continues past it.
    Middle,
    /// The box is the end of the chain.
    Exit,
}

impl HopRole {
    /// The role a box plays at `index` in a chain of `length` boxes.
    ///
    /// The first box receives the client, the last one exits, and everything
    /// between relays onward. A chain of one box is the exit: it receives
    /// the client and exits by itself.
    pub fn for_position(index: usize, length: usize) -> Self {
        if length <= 1 {
            HopRole::Exit
        } else if index == 0 {
            HopRole::Entry
        } else if index + 1 == length {
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

/// One door of a box, as the box published it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainFace {
    /// `host:port` the box published this door on.
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
    /// The door the chain leg to this box uses.
    pub face: ChainFace,
}

/// A whole chain, as the client keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chain {
    /// The chain's id — the same in every box.
    pub chain_id: String,
    /// The boxes, in the order the traffic walks them.
    pub boxes: Vec<ChainBox>,
}

impl Chain {
    /// The role of the box at `index`.
    pub fn role_of(&self, index: usize) -> HopRole {
        HopRole::for_position(index, self.boxes.len())
    }

    /// The door the CLIENT dials: the first box's own published door.
    ///
    /// A client that joins the chain walks the same road as everybody else,
    /// so it starts where the chain starts.
    pub fn entry_face(&self) -> Option<&ChainFace> {
        self.boxes.first().map(|node| &node.face)
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
        let mut env = String::new();
        if !base.trim().is_empty() {
            env.push_str(base.trim_end());
            env.push('\n');
        }
        env.push_str(&format!("MIRAGE_CHAIN_ID={}\n", self.chain_id));
        env.push_str(&format!("MIRAGE_CHAIN_POSITION={index}\n"));
        env.push_str(&format!("MIRAGE_CHAIN_LENGTH={}\n", self.boxes.len()));
        for (hop, node) in self.boxes.iter().enumerate() {
            env.push_str(&format!("MIRAGE_CHAIN_HOP_{hop}_ADDR={}\n", node.face.addr));
            env.push_str(&format!(
                "MIRAGE_CHAIN_HOP_{hop}_PUBKEY={}\n",
                node.face.pubkey_hex
            ));
            env.push_str(&format!(
                "MIRAGE_CHAIN_HOP_{hop}_MASK={}\n",
                one_line(&node.face.camouflage)?
            ));
        }
        Ok(env)
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

    fn chain() -> Chain {
        Chain {
            chain_id: "stand-2hop".to_owned(),
            boxes: vec![
                ChainBox {
                    host: "144.172.115.106".to_owned(),
                    user: "root".to_owned(),
                    face: face("144.172.115.106:443"),
                },
                ChainBox {
                    host: "144.172.105.29".to_owned(),
                    user: "root".to_owned(),
                    face: face("144.172.105.29:443"),
                },
            ],
        }
    }

    #[test]
    fn the_first_box_is_the_entry_and_the_last_is_the_exit() {
        let chain = chain();
        assert_eq!(chain.role_of(0), HopRole::Entry);
        assert_eq!(chain.role_of(1), HopRole::Exit);
    }

    #[test]
    fn a_chain_of_one_box_is_the_exit() {
        let chain = Chain {
            chain_id: "solo".to_owned(),
            boxes: vec![ChainBox {
                host: "10.0.0.1".to_owned(),
                user: "root".to_owned(),
                face: face("10.0.0.1:443"),
            }],
        };
        assert_eq!(chain.role_of(0), HopRole::Exit);
    }

    #[test]
    fn a_middle_box_is_a_middle_box() {
        let chain = Chain {
            chain_id: "three".to_owned(),
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
            ],
        };
        assert_eq!(chain.role_of(0), HopRole::Entry);
        assert_eq!(chain.role_of(1), HopRole::Middle);
        assert_eq!(chain.role_of(2), HopRole::Exit);
    }

    #[test]
    fn every_box_gets_the_whole_chain_and_its_own_position() {
        let chain = chain();
        let first = chain.server_env(0, "STACK=auto\n").expect("env");
        let second = chain.server_env(1, "STACK=auto\n").expect("env");

        assert!(first.contains("MIRAGE_CHAIN_POSITION=0"));
        assert!(second.contains("MIRAGE_CHAIN_POSITION=1"));
        assert!(first.contains("MIRAGE_CHAIN_LENGTH=2"));
        assert!(second.contains("MIRAGE_CHAIN_LENGTH=2"));

        // Both boxes are told about BOTH hops — that is the whole point.
        for env in [&first, &second] {
            assert!(env.contains("MIRAGE_CHAIN_HOP_0_ADDR=144.172.115.106:443"));
            assert!(env.contains("MIRAGE_CHAIN_HOP_1_ADDR=144.172.105.29:443"));
            assert!(env.contains(&format!("MIRAGE_CHAIN_HOP_0_PUBKEY={}", "22".repeat(32))));
            assert!(env.contains(&format!("MIRAGE_CHAIN_HOP_1_PUBKEY={}", "22".repeat(32))));
            assert!(env.contains("MIRAGE_CHAIN_HOP_0_MASK="));
            assert!(env.contains("MIRAGE_CHAIN_HOP_1_MASK="));
        }
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
        chain.boxes[0].face.camouflage = "{\n\"type\":\"tls_real\"\n}".to_owned();
        assert_eq!(
            chain.server_env(0, "").expect_err("two lines"),
            ChainError::MaskNotOneLine
        );
    }
}
