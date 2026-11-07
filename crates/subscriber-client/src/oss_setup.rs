//! The whole self-assembly path: from nothing to a client that works.
//!
//! # The path
//!
//! 1. the client generates its **own** key pair — in this mode the client IS
//!    the subscriber, and there is nobody else to issue one;
//! 2. the box is installed with that key pair's **public** key in its
//!    allowlist, so the box answers to this client and to nobody else;
//! 3. the box publishes itself;
//! 4. the artefact is built from the publication — the very same artefact the
//!    fleet's panel issues;
//! 5. the artefact is written down and the data plane takes it.
//!
//! One command, one artefact, one client: the same data plane the customer
//! uses with the cabinet mesh.
//!
//! # What never leaves this machine
//!
//! The client's secret key: it is written to the file the operator names (or
//! kept in memory when no file is named) and nowhere else. The box's own key
//! never leaves the box — it is generated there, in the install script.

use std::path::{Path, PathBuf};

use mirage_client_chain::oss::OssProposal;
use mirage_client_chain::oss_install::{OssCarrier, OssJob, OssOutcome};
use mirage_client_chain::provision::InstallBus;
use mirage_core::crypto::KeyPair;

use crate::artefact::EntranceArtefact;
use crate::oss_artefact::{build, client_public_key_b64};

/// What the self-assembly needs to know.
#[derive(Debug, Clone)]
pub struct OssSetup {
    /// The box, as `user@host:port`.
    pub box_addr: String,
    /// The password the provider gave. It is used once, to plant the ssh key.
    pub password: String,
    /// Where the `mirage-vpn` binary is on this machine.
    pub binary: PathBuf,
    /// The carriers the box opens.
    pub carriers: Vec<OssCarrier>,
    /// The id both ends bake into the handshake.
    pub mesh_id: String,
    /// The name the client goes under in the box's peer table.
    pub client_name: String,
    /// The label the artefact carries.
    pub label: String,
    /// This box's own state directory: the ssh key and the host-key pin.
    pub state: Option<PathBuf>,
    /// The ssh key the client uses for this box's door.
    ///
    /// The operator's own key — the one the box already knows — is the right
    /// one: the box is reachable by the key that is written down in the
    /// inventory, and the client does not get a second door into the same box.
    /// The public half is the same path with `.pub` beside it.
    pub ssh_key: Option<PathBuf>,
    /// Where the client's own key pair is kept between runs. Without it the
    /// key pair is made fresh every time, and the box's allowlist has to be
    /// rewritten for every run.
    pub key_path: Option<PathBuf>,
}

/// What the self-assembly gives back.
#[derive(Debug, Clone)]
pub struct OssSetupOutcome {
    /// The artefact the data plane takes.
    pub artefact: EntranceArtefact,
    /// The artefact as JSON — what the client writes down.
    pub artefact_json: String,
    /// The publication the box made about itself.
    pub proposal: OssProposal,
    /// The client's public key, base64 — what went into the allowlist.
    pub client_public_key: String,
    /// A warning that is not a failure.
    pub warn: Option<String>,
}

/// Install the box, read back what it published, and build the artefact.
pub async fn setup(setup: OssSetup, bus: &InstallBus) -> Result<OssSetupOutcome, String> {
    let (user, host, port) = parse_box(&setup.box_addr)?;
    let keypair = load_or_make_keypair(setup.key_path.as_deref())?;
    let client_public_key = client_public_key_b64(&keypair);

    let job = OssJob {
        host: host.clone(),
        port,
        user,
        password: setup.password.clone(),
        client_public_key: client_public_key.clone(),
        client_name: setup.client_name.clone(),
        mesh_id: setup.mesh_id.clone(),
        binary: setup.binary.clone(),
        carriers: setup.carriers.clone(),
        state: setup.state.clone(),
        ssh_key: setup.ssh_key.clone(),
    };

    let (proposal, warn) = match mirage_client_chain::oss_install::install(job, bus).await? {
        OssOutcome::Published { proposal, warn } => (proposal, warn),
    };

    let artefact = build(&proposal, &keypair, &host, &setup.label, now_ms())
        .map_err(|err| format!("the artefact did not build: {err}"))?;
    let artefact_json = artefact
        .to_json()
        .map_err(|err| format!("the artefact did not serialize: {err}"))?;

    Ok(OssSetupOutcome {
        artefact,
        artefact_json,
        proposal,
        client_public_key,
        warn,
    })
}

/// Split `user@host:port` into its parts.
pub fn parse_box(addr: &str) -> Result<(String, String, u16), String> {
    let trimmed = addr.trim();
    let (user, rest) = match trimmed.split_once('@') {
        Some((user, rest)) => (user.trim().to_owned(), rest.trim()),
        None => ("root".to_owned(), trimmed),
    };
    if user.is_empty() {
        return Err("the box has no user".to_owned());
    }
    // The port may be given as `host:port` — and an IPv6 address carries
    // colons of its own, so the port is taken from the last colon.
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .map_err(|_| format!("the box's port is not a number: {port}"))?;
            (host.trim_matches(['[', ']']).to_owned(), port)
        }
        None => (rest.to_owned(), 22),
    };
    if host.is_empty() {
        return Err("the box has no address".to_owned());
    }
    Ok((user, host, port))
}

/// The client's own key pair — the same one on every run.
///
/// Without a file to keep it in, the pair is made fresh and the box's
/// allowlist has to follow it. With a file, the pair is read back and the
/// box keeps admitting the same client.
pub fn load_or_make_keypair(path: Option<&Path>) -> Result<KeyPair, String> {
    let Some(path) = path else {
        return Ok(KeyPair::generate());
    };
    if path.is_file() {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("the key file: {e}"))?;
        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("the key file is not JSON: {e}"))?;
        let secret_hex = value
            .get("secret_hex")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "the key file carries no secret".to_owned())?;
        let bytes = hex::decode(secret_hex).map_err(|e| format!("the key file's secret: {e}"))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "the key file's secret is not 32 bytes".to_owned())?;
        return Ok(KeyPair::from_secret(bytes));
    }
    let keypair = KeyPair::generate();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("the key file's dir: {e}"))?;
    }
    let document = serde_json::json!({
        "secret_hex": hex::encode(keypair.secret.to_bytes()),
        "public_b64": client_public_key_b64(&keypair),
    });
    let text = serde_json::to_string_pretty(&document)
        .map_err(|e| format!("the key file did not serialize: {e}"))?;
    std::fs::write(path, text).map_err(|e| format!("the key file: {e}"))?;
    Ok(keypair)
}

/// The clock the artefact is stamped with.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_box_is_split_into_its_parts() {
        let (user, host, port) = parse_box("root@203.0.113.9:2222").expect("a box");
        assert_eq!(user, "root");
        assert_eq!(host, "203.0.113.9");
        assert_eq!(port, 2222);
    }

    #[test]
    fn a_box_without_a_port_gets_the_default_one() {
        let (user, host, port) = parse_box("root@203.0.113.9").expect("a box");
        assert_eq!(user, "root");
        assert_eq!(host, "203.0.113.9");
        assert_eq!(port, 22);
    }

    #[test]
    fn a_box_without_a_user_is_root() {
        let (user, host, _) = parse_box("203.0.113.9:2222").expect("a box");
        assert_eq!(user, "root");
        assert_eq!(host, "203.0.113.9");
    }

    #[test]
    fn an_ipv6_box_keeps_its_address() {
        let (user, host, port) = parse_box("root@[2001:db8::1]:2222").expect("a box");
        assert_eq!(user, "root");
        assert_eq!(host, "2001:db8::1");
        assert_eq!(port, 2222);
    }

    #[test]
    fn a_box_with_no_address_is_refused() {
        assert!(parse_box("root@:2222").is_err());
        assert!(parse_box("").is_err());
    }

    #[test]
    fn the_key_pair_is_the_same_on_the_second_run() {
        let dir = std::env::temp_dir().join("mirage-oss-setup-test");
        std::fs::create_dir_all(&dir).expect("the dir");
        let path = dir.join("client.key.json");
        let _ = std::fs::remove_file(&path);

        let first = load_or_make_keypair(Some(&path)).expect("the first key");
        assert!(path.is_file(), "the key must be written down");
        let second = load_or_make_keypair(Some(&path)).expect("the second key");
        assert_eq!(
            first.secret.to_bytes(),
            second.secret.to_bytes(),
            "the second run must read the same key back"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn without_a_file_the_key_pair_is_fresh() {
        let first = load_or_make_keypair(None).expect("a key");
        let second = load_or_make_keypair(None).expect("a key");
        assert_ne!(first.secret.to_bytes(), second.secret.to_bytes());
    }
}
