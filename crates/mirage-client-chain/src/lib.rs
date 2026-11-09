//! Chain provisioning — the part of the client that BUILDS a chain of boxes.
//!
//! # Why this lives in its own crate
//!
//! A chain is not a GUI feature: it is a way to stand up a relay path of
//! several boxes and wire them into one road. The desktop client builds one
//! from its Setup screen, the headless client builds one from a command
//! line, and both need the SAME chain — the same env the edge is told, the
//! same key material, the same doors. Two copies of that would drift, and a
//! chain built by one of them would not be readable by the other.
//!
//! So the chain lives here, and both clients are consumers:
//!
//! - [`chain`] — what a chain IS: the boxes, the hop list, the env each box
//!   is given.
//! - [`provision`] — how a box is installed: password once, an ed25519 key
//!   after that, then Docker, then the face proposal.
//! - [`oss_install`] — how a SELF-ASSEMBLED box is installed: the same door
//!   and the same key, but the OSS `mirage-vpn` binary instead of Docker, and
//!   the publication the box makes about itself.
//! - [`chain_install`] — putting several boxes together into one chain.
//! - [`i18n`] — the copy of the installer's status lines, in Russian and
//!   English. It travels WITH the installer, so a headless client speaks the
//!   same words as the GUI.
//!
//! Nothing here knows about windows, buttons or the client GUI: it is plain
//! library code, and it runs the same on a customer's machine as it does in
//! a headless container.

pub mod chain;
pub mod chain_install;
pub mod i18n;
pub mod oss;
pub mod oss_install;
pub mod provision;

pub use chain::{Chain, ChainBox, ChainError, ChainFace, ChainKeys, ChainRole, HopRole};
pub use chain_install::{
    assemble_chain, install_chain, save_chain, ChainOutcome, ChainRequest, InstalledBox,
};
pub use oss::{OssFace, OssProposal, PROPOSAL_VERSION};
pub use oss_install::{OssCarrier, OssJob, OssOutcome};
pub use provision::{InstallBus, Job, Outcome, ProposedFace};
