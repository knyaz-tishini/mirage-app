//! The automatic install of a self-assembled box.
//!
//! # What this is
//!
//! The fleet's box is installed with Docker: an image, a compose file, a
//! volume. A self-assembled box is the OSS `mirage-vpn`: a binary, a JSON
//! config, a process. This module is the **second install path** — it stands
//! next to the fleet's one, it does not replace it:
//!
//! 1. the operator names the box (host, port, user) and the password it has
//!    from the provider;
//! 2. the client's key is planted on the box and the password door is closed;
//! 3. the `mirage-vpn` binary goes up;
//! 4. the config is written — **with the client's own public key in the box's
//!    allowlist**, so the box answers to this client and to nobody else;
//! 5. the server starts;
//! 6. the box publishes itself (`mirage-vpn propose`) and that publication is
//!    read back and returned.
//!
//! # What the operator never sees
//!
//! The box's own private key is generated **on the box** and never leaves it.
//! What the operator carries is the client's *public* key — it goes into the
//! allowlist and nowhere else.
//!
//! # The publication
//!
//! The box prints the publication between two markers, because the box says
//! other things on the same stream. A publication that swallowed a banner
//! would be a publication nobody could trust.

use std::fs;
use std::path::{Path, PathBuf};

use crate::oss::OssProposal;
use crate::provision::{self, InstallBus};

/// Where the OSS stack lives on the box.
pub const REMOTE_DIR: &str = "/opt/mirage-vpn";

/// The marker the publication is printed after.
const PUBLICATION_BEGIN: &str = "mirage-oss-publication-begin";

/// The marker the publication is printed before.
const PUBLICATION_END: &str = "mirage-oss-publication-end";

/// The placeholder the box's own key is written into.
///
/// The key is generated on the box and never travels, so the config that goes
/// up carries a placeholder and the box fills it in.
const SERVER_KEY_PLACEHOLDER: &str = "__MIRAGE_SERVER_KEY__";

/// The id both ends bake into the handshake, when the operator names none.
pub const DEFAULT_MESH_ID: &str = "mirage-vpn";

/// A carrier a box opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OssCarrier {
    /// The MirageVG carrier: one UDP port with a junk burst in front.
    Mwg {
        /// The port the carrier sits on.
        port: u16,
        /// The marker in front of every packet.
        magic_byte: u8,
        /// How many junk datagrams go out before the handshake.
        junk_count: u8,
        /// The smallest junk datagram.
        junk_min: u16,
        /// The largest junk datagram.
        junk_max: u16,
    },
    /// The MirageKVIC carrier: one QUIC endpoint.
    Mkvik {
        /// The port the carrier sits on.
        port: u16,
        /// The ALPN the endpoint speaks.
        alpn: Vec<String>,
    },
}

impl OssCarrier {
    /// The MirageVG carrier with the defaults the box itself uses.
    pub fn mwg(port: u16) -> Self {
        OssCarrier::Mwg {
            port,
            magic_byte: 1,
            junk_count: 4,
            junk_min: 10,
            junk_max: 20,
        }
    }

    /// The MirageKVIC carrier with the default ALPN.
    pub fn mkvik(port: u16) -> Self {
        OssCarrier::Mkvik {
            port,
            alpn: vec!["h3".to_owned()],
        }
    }

    /// The port the carrier sits on.
    pub fn port(&self) -> u16 {
        match self {
            OssCarrier::Mwg { port, .. } => *port,
            OssCarrier::Mkvik { port, .. } => *port,
        }
    }
}

/// What the install of a self-assembled box needs.
#[derive(Debug, Clone)]
pub struct OssJob {
    /// The box's address.
    pub host: String,
    /// The box's ssh port.
    pub port: u16,
    /// The user on the box.
    pub user: String,
    /// The password the provider gave. It is used once, to plant the key.
    pub password: String,
    /// The client's own static public key, base64 — it goes into the box's
    /// allowlist, and that is the only place it goes.
    pub client_public_key: String,
    /// The name that peer goes under, for the operator's eyes.
    pub client_name: String,
    /// The id both ends bake into the handshake.
    pub mesh_id: String,
    /// Where the `mirage-vpn` binary is on THIS machine.
    pub binary: PathBuf,
    /// The carriers the box opens.
    pub carriers: Vec<OssCarrier>,
    /// This box's own state directory: the client's ssh key and the host-key
    /// pin live there, and a chain gives every box its own.
    pub state: Option<PathBuf>,
    /// The ssh key the client uses for this box's door.
    ///
    /// The operator's own key — the one the box already knows — is the right
    /// one: the box has to be reachable by the same key that is written down
    /// in the inventory, and the client's key is then not a second door into
    /// the same box. The public half is the same path with `.pub` beside it.
    ///
    /// Without it the installer mints a key of its own in the state directory
    /// and plants it with the password.
    pub ssh_key: Option<PathBuf>,
}

/// What the install of a self-assembled box gives back.
#[derive(Debug, Clone)]
pub enum OssOutcome {
    /// The box is up and published itself.
    Published {
        /// What the box published.
        proposal: OssProposal,
        /// What the operator should know that is not a failure.
        warn: Option<String>,
    },
}

/// Install a self-assembled box and read back what it published.
pub async fn install(job: OssJob, bus: &InstallBus) -> Result<OssOutcome, String> {
    if job.client_public_key.trim().is_empty() {
        return Err(crate::i18n::tr("p.oss_no_client_key").to_owned());
    }
    if job.carriers.is_empty() {
        return Err(crate::i18n::tr("p.oss_no_carrier").to_owned());
    }

    let dir = job_state_dir(&job)?;
    let (key_path, pub_path) = match job.ssh_key.as_ref() {
        // The operator's own key for this box: the box already knows it, and
        // the client does not get a second door into the same box.
        Some(path) => {
            // The public half is the same name with `.pub` beside it — the
            // name the inventory writes, dot and all.
            let mut name = path.clone().into_os_string();
            name.push(".pub");
            (path.clone(), PathBuf::from(name))
        }
        None => {
            let key = crate::oss_install::oss_key(job.state.as_deref())?;
            (key.priv_path, key.pub_path)
        }
    };
    provision::ensure_key(&key_path, &pub_path)?;
    let pub_line = fs::read_to_string(&pub_path)
        .map_err(|e| format!("{}: {e}", crate::i18n::tr("p.no_key")))?
        .trim()
        .to_owned();
    let pem = fs::read(&key_path).map_err(|e| format!("{}: {e}", crate::i18n::tr("p.no_key")))?;
    let pins = dir.join("host-keys.json");
    let addr = format!("{}:{}", job.host, job.port);

    // The box may already know the client's key — it does after the first
    // install. The password is only used to plant the key on a box that has
    // never seen it.
    let session = match provision::connect_key(&addr, &job.user, &pem, &pins).await {
        Ok(session) => session,
        Err(key_error) => {
            if job.password.is_empty() {
                return Err(key_error);
            }
            provision::set_text(bus, crate::i18n::tr("p.key_on"));
            let password = job.password.clone();
            let planted = provision::password_install_key_when_ready(
                &addr,
                &job.user,
                &password,
                &pub_line,
                &pins,
                bus,
            )
            .await;
            drop(password);
            planted?;
            provision::connect_key(&addr, &job.user, &pem, &pins).await?
        }
    };

    // The password door: closed before anything else goes up.
    let mut warn = provision::disable_password_auth(&session).await.err();

    // The binary.
    let bytes = fs::read(&job.binary).map_err(|e| format!("{}: {e}", crate::i18n::tr("p.oss_binary")))?;
    provision::exec(&session, &format!("mkdir -p {REMOTE_DIR}")).await?;
    provision::upload(&session, &bytes, &format!("{REMOTE_DIR}/mirage-vpn")).await?;

    // The config, with the client's public key in the allowlist.
    let config = server_config(&job);
    provision::upload(
        &session,
        config.as_bytes(),
        &format!("{REMOTE_DIR}/server.json.tmpl"),
    )
    .await?;
    provision::upload(
        &session,
        install_sh().as_bytes(),
        &format!("{REMOTE_DIR}/install.sh"),
    )
    .await?;

    // The install itself.
    provision::set_text(bus, crate::i18n::tr("p.oss_installing"));
    let out = provision::exec(&session, &format!("sh {REMOTE_DIR}/install.sh")).await?;
    if out.exit.unwrap_or(1) != 0 {
        let reason = if out.stderr.trim().is_empty() {
            out.stdout.trim().to_owned()
        } else {
            out.stderr.trim().to_owned()
        };
        return Err(format!("{}: {reason}", crate::i18n::tr("p.oss_install_failed")));
    }

    let proposal = parse_publication(&out.stdout)?;
    provision::set_text(bus, crate::i18n::tr("p.oss_up"));
    Ok(OssOutcome::Published { proposal, warn })
}

/// Read the publication out of what the box printed.
///
/// The publication sits between two markers. Anything the box said around it
/// — a banner, a warning — is not part of the publication and must not be
/// mistaken for it.
pub fn parse_publication(stdout: &str) -> Result<OssProposal, String> {
    let start = stdout
        .find(PUBLICATION_BEGIN)
        .ok_or_else(|| crate::i18n::tr("p.oss_no_publication").to_owned())?;
    let after = &stdout[start + PUBLICATION_BEGIN.len()..];
    let end = after
        .find(PUBLICATION_END)
        .ok_or_else(|| crate::i18n::tr("p.oss_no_publication").to_owned())?;
    let body = after[..end].trim();
    OssProposal::parse(body)
}

/// The config the box is given.
///
/// The private key is a placeholder: the box fills it in with a key it
/// generates itself.
pub fn server_config(job: &OssJob) -> String {
    let mut listeners = serde_json::Map::new();
    for carrier in &job.carriers {
        match carrier {
            OssCarrier::Mwg {
                port,
                magic_byte,
                junk_count,
                junk_min,
                junk_max,
            } => {
                listeners.insert(
                    "mwg".to_owned(),
                    serde_json::json!({
                        "bind": format!("0.0.0.0:{port}"),
                        "magic_byte": magic_byte,
                        "junk_count": junk_count,
                        "junk_min": junk_min,
                        "junk_max": junk_max,
                    }),
                );
            }
            OssCarrier::Mkvik { port, alpn } => {
                listeners.insert(
                    "mkvik".to_owned(),
                    serde_json::json!({
                        "bind": format!("0.0.0.0:{port}"),
                        "alpn": alpn,
                    }),
                );
            }
        }
    }
    let mesh_id = if job.mesh_id.trim().is_empty() {
        DEFAULT_MESH_ID
    } else {
        job.mesh_id.trim()
    };
    let config = serde_json::json!({
        "private_key": SERVER_KEY_PLACEHOLDER,
        "peers": [{
            "public_key": job.client_public_key.trim(),
            "name": job.client_name.trim(),
        }],
        "listeners": serde_json::Value::Object(listeners),
        "mesh_id": mesh_id,
        "exit": { "mode": "direct" },
    });
    serde_json::to_string_pretty(&config).unwrap_or_else(|_| config.to_string())
}

/// The script the box runs.
///
/// It writes the config with a key the box generates itself, starts the
/// server, and prints the publication between the markers.
pub fn install_sh() -> String {
    format!(
        r#"#!/bin/sh
set -eu
DIR={REMOTE_DIR}
mkdir -p "$DIR"
chmod 0755 "$DIR/mirage-vpn"
# The box's own key is generated HERE and stays here: it never travels.
if [ ! -f "$DIR/server.key" ]; then
  head -c 32 /dev/urandom | base64 | tr -d '\n' > "$DIR/server.key"
  chmod 0600 "$DIR/server.key"
fi
KEY=$(cat "$DIR/server.key")
sed "s|{SERVER_KEY_PLACEHOLDER}|$KEY|" "$DIR/server.json.tmpl" > "$DIR/server.json"
chmod 0600 "$DIR/server.json"
rm -f "$DIR/server.json.tmpl"
# A box that was installed before is replaced, not doubled.
pkill -x mirage-vpn 2>/dev/null || true
sleep 1
( cd "$DIR" && setsid ./mirage-vpn serve --config "$DIR/server.json" > "$DIR/server.log" 2>&1 < /dev/null ) > /dev/null 2>&1 &
sleep 2
if ! pgrep -x mirage-vpn > /dev/null 2>&1; then
  echo "the box did not come up" >&2
  tail -n 40 "$DIR/server.log" >&2 || true
  exit 21
fi
echo "{PUBLICATION_BEGIN}"
"$DIR/mirage-vpn" propose --config "$DIR/server.json"
echo "{PUBLICATION_END}"
"#
    )
}

/// The files the client's own key for a box lives in.
#[derive(Debug, Clone)]
pub struct OssKey {
    /// The private half. It never leaves this machine.
    pub priv_path: PathBuf,
    /// The public half. This is what the box takes.
    pub pub_path: PathBuf,
}

/// The client's own key for a box, in the state directory.
///
/// The state directory is the one the job names, or the default beside the
/// executable. The key is made once and kept: the same client comes back to
/// the same box with the same key, and the box's allowlist does not have to
/// follow a fresh one every time.
///
/// The person can put the public half into the box by hand — through the
/// hoster's panel, or with a single `authorized_keys` line — and the client
/// comes in by key from then on, with no password at all.
pub fn oss_key(state: Option<&Path>) -> Result<OssKey, String> {
    let dir = match state {
        Some(dir) => {
            fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
            dir.to_path_buf()
        }
        None => default_state_dir()?,
    };
    Ok(OssKey {
        // The public half is the same name with `.pub` beside it — the
        // OpenSSH way, and the way the operator's own keys are named. A key
        // brought in from outside is read the same way, so one rule holds
        // for both.
        priv_path: dir.join("server"),
        pub_path: dir.join("server.pub"),
    })
}

/// The state directory one job works in.
///
/// A job that names its own directory gets it — a chain gives every box its
/// own, so the key the client installs on one box and the host-key pin of
/// that box are never overwritten by the next box in the chain.
fn job_state_dir(job: &OssJob) -> Result<PathBuf, String> {
    match job.state.as_ref() {
        Some(dir) => {
            fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
            Ok(dir.clone())
        }
        None => default_state_dir(),
    }
}

/// Where the state lives when the job names no directory.
fn default_state_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("exe: {e}"))?;
    let dir = exe
        .parent()
        .ok_or("exe dir")?
        .join("standalone")
        .join("oss");
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    Ok(dir)
}

/// The binary the job names, if it is there.
pub fn binary_path(job: &OssJob) -> Option<&Path> {
    job.binary.is_file().then_some(job.binary.as_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job, without a box behind it.
    fn job() -> OssJob {
        OssJob {
            host: "127.0.0.1".to_owned(),
            port: 22,
            user: "root".to_owned(),
            password: "secret".to_owned(),
            client_public_key: "9LXEwZ5d0gBielXrVJ41AZV0xRRIb1MMW12LDHkFQhU=".to_owned(),
            client_name: "customer".to_owned(),
            mesh_id: "mirage-vpn".to_owned(),
            binary: PathBuf::from("/tmp/mirage-vpn"),
            carriers: vec![OssCarrier::mwg(41443), OssCarrier::mkvik(41444)],
            state: None,
            ssh_key: None,
        }
    }

    #[test]
    fn the_config_carries_the_client_key_in_the_allowlist() {
        let config = server_config(&job());
        let parsed: serde_json::Value = serde_json::from_str(&config).expect("the config is JSON");
        let peers = parsed["peers"].as_array().expect("the peers");
        assert_eq!(peers.len(), 1);
        assert_eq!(
            peers[0]["public_key"].as_str(),
            Some("9LXEwZ5d0gBielXrVJ41AZV0xRRIb1MMW12LDHkFQhU=")
        );
        assert_eq!(peers[0]["name"].as_str(), Some("customer"));
    }

    #[test]
    fn the_config_never_carries_a_real_key() {
        let config = server_config(&job());
        assert!(config.contains(SERVER_KEY_PLACEHOLDER));
        // The box's own key is generated on the box: the config that travels
        // carries a placeholder and nothing else.
        assert!(!config.contains("private_key\": \"ey"));
    }

    #[test]
    fn the_config_opens_the_carriers_it_was_given() {
        let parsed: serde_json::Value = serde_json::from_str(&server_config(&job())).expect("JSON");
        assert_eq!(
            parsed["listeners"]["mwg"]["bind"].as_str(),
            Some("0.0.0.0:41443")
        );
        assert_eq!(
            parsed["listeners"]["mkvik"]["bind"].as_str(),
            Some("0.0.0.0:41444")
        );
        assert_eq!(parsed["listeners"]["mkvik"]["alpn"][0].as_str(), Some("h3"));
        assert_eq!(parsed["exit"]["mode"].as_str(), Some("direct"));
    }

    #[test]
    fn the_mesh_id_falls_back_to_the_default() {
        let mut job = job();
        job.mesh_id = "  ".to_owned();
        let parsed: serde_json::Value = serde_json::from_str(&server_config(&job)).expect("JSON");
        assert_eq!(parsed["mesh_id"].as_str(), Some(DEFAULT_MESH_ID));
    }

    #[test]
    fn the_script_generates_the_key_on_the_box() {
        let script = install_sh();
        assert!(script.contains("head -c 32 /dev/urandom"));
        assert!(script.contains(SERVER_KEY_PLACEHOLDER));
        assert!(script.contains(PUBLICATION_BEGIN));
        assert!(script.contains(PUBLICATION_END));
        assert!(script.contains("propose --config"));
    }

    #[test]
    fn the_publication_is_read_out_of_what_the_box_printed() {
        let printed = format!(
            "the server is up\n{PUBLICATION_BEGIN}\n{{\"v\":1,\"server_public_key_hex\":\"aa\",\"mesh_id\":\"mirage-vpn\",\"faces\":[]}}\n{PUBLICATION_END}\n"
        );
        let proposal = parse_publication(&printed).expect("the publication reads");
        assert_eq!(proposal.v, 1);
        assert_eq!(proposal.mesh_id, "mirage-vpn");
    }

    #[test]
    fn a_box_that_printed_no_publication_is_refused() {
        assert!(parse_publication("the server is up\n").is_err());
        assert!(parse_publication("").is_err());
    }

    #[test]
    fn the_carrier_defaults_are_the_box_ones() {
        let mwg = OssCarrier::mwg(41443);
        assert_eq!(
            mwg,
            OssCarrier::Mwg {
                port: 41443,
                magic_byte: 1,
                junk_count: 4,
                junk_min: 10,
                junk_max: 20,
            }
        );
        assert_eq!(OssCarrier::mkvik(41444).port(), 41444);
    }
}
