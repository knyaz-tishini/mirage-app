//! Standalone server install. The desktop client captures a host the way a
//! self-hosted client does: password once, an ed25519 key after that, then
//! Docker.
//!
//! The image contract the container must meet (`m1r4g3-server:1`, or the
//! tar `m1r4g3-server.tar` beside this exe, which is `docker load`ed):
//!
//! - read `/opt/m1r4g3/server.env` (`STACK=auto` or a chosen stack id)
//! - keep state in the `m1r4g3-data` volume mounted at `/data`
//! - write one line to `/data/client.link` — the text other clients scan
//!
//! The password is moved into the worker and dropped after the key is on
//! the host. It is not written to disk and not put in the status line.
//!
//! # Two steps, in this order
//!
//! The set of faces a box publishes is a **human** decision, so the install
//! is two steps and not one:
//!
//! 1. **Ask.** The box is asked what it could publish — which ports are
//!    free on it — and answers with a proposal. Nothing is published yet.
//! 2. **Choose.** The person looks at the proposal, turns the faces they do
//!    not want off, and the install goes on with what is left.
//!
//! The first press of "install" does step 1 and stops there; the second one
//! does step 2. The image, the compose file, the volume and the
//! `m1r4g3-link <LINK>` line are the same in both.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::rngs::OsRng;
use russh::client;
use russh::keys::ssh_key::PublicKey;
use russh_keys::key::PrivateKeyWithHashAlg;
use russh_sftp::client::SftpSession;
use sha2::Digest;
use ssh_key::{Algorithm, LineEnding, PrivateKey};
use tokio::io::AsyncWriteExt;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The marker the face proposal is printed between, so that whatever else
/// the box says on stdout cannot be mistaken for a proposal.
const FACES_BEGIN: &str = "m1r4g3-faces-begin";
/// The closing marker of the proposal block.
const FACES_END: &str = "m1r4g3-faces-end";

/// One face the box offered, as read from the box's proposal file.
///
/// The mask is the JSON the server's `TransportCamouflage` serializes to,
/// kept as text: the client does not need to understand it, it only writes
/// it back into `server.env` unchanged.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProposedFace {
    /// The port the face would sit on.
    pub port: u16,
    /// The transport: `tcp` or `udp`.
    pub transport: String,
    /// The mask, as JSON.
    pub mask: String,
    /// Why the box offered this face.
    pub why: String,
}

/// The proposal file the box writes.
#[derive(Debug, Clone, serde::Deserialize)]
struct FaceProposal {
    /// One entry per face.
    #[serde(rename = "face")]
    faces: Vec<ProposedFace>,
}

pub struct InstallBus {
    pub running: std::sync::atomic::AtomicBool,
    pub text: Mutex<String>,
    pub link: Mutex<Option<String>>,
    /// The faces the box offered, waiting for the person to choose.
    pub faces: Mutex<Option<Vec<ProposedFace>>>,
}

impl InstallBus {
    pub fn new() -> Self {
        Self {
            running: std::sync::atomic::AtomicBool::new(false),
            text: Mutex::new(String::new()),
            link: Mutex::new(None),
            faces: Mutex::new(None),
        }
    }
}

pub struct Job {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub stack: String,
    /// Full `server.env` body. Empty keeps the single `STACK=` line.
    pub env: String,
    /// Ask the box what it could publish and stop there.
    ///
    /// The first press of "install" asks; the second one installs with the
    /// faces the person chose.
    pub ask_faces: bool,
}

/// What one install step ended with.
enum Outcome {
    /// The box answered what it could offer. The person now chooses.
    Faces {
        faces: Vec<ProposedFace>,
        /// What the operator should know that is not a failure.
        warn: Option<String>,
    },
    /// The box is up, and this is the customer's link.
    Link {
        link: String,
        /// What the operator should know that is not a failure.
        warn: Option<String>,
    },
}

pub fn start(job: Job, bus: Arc<InstallBus>) {
    use std::sync::atomic::Ordering;
    if bus.running.swap(true, Ordering::Relaxed) {
        return;
    }
    set_text(&bus, crate::text::tr("p.connect"));
    std::thread::spawn(move || {
        let outcome = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(install(job, &bus)),
            Err(err) => Err(format!("runtime: {err}")),
        };
        match outcome {
            Ok(Outcome::Faces { faces, warn }) => {
                *bus.faces.lock().unwrap_or_else(|p| p.into_inner()) = Some(faces);
                set_text(&bus, &status_line(crate::text::tr("p.faces_review"), warn));
            }
            Ok(Outcome::Link { link, warn }) => {
                *bus.link.lock().unwrap_or_else(|p| p.into_inner()) = Some(link);
                set_text(&bus, &status_line(crate::text::tr("p.up"), warn));
            }
            Err(err) => set_text(&bus, &err),
        }
        bus.running.store(false, Ordering::Relaxed);
    });
}

/// The status line, with a warning appended when there is one.
///
/// A warning is not a failure: the install stands, but the operator has to
/// know about it — a box that kept its password door is a box the customer
/// can still walk into with a password.
fn status_line(base: &str, warn: Option<String>) -> String {
    match warn {
        Some(warn) => format!("{base} — {warn}"),
        None => base.to_owned(),
    }
}

/// Key-only `docker compose down`. No password. Shares the install bus so the
/// two jobs cannot run together.
pub fn remove(host: String, port: u16, user: String, bus: Arc<InstallBus>) {
    use std::sync::atomic::Ordering;
    if bus.running.swap(true, Ordering::Relaxed) {
        return;
    }
    set_text(&bus, crate::text::tr("p.removing"));
    std::thread::spawn(move || {
        let outcome = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(take_down(&host, port, &user)),
            Err(err) => Err(format!("runtime: {err}")),
        };
        match outcome {
            Ok(()) => set_text(&bus, crate::text::tr("p.removed")),
            Err(err) => set_text(&bus, &err),
        }
        bus.running.store(false, Ordering::Relaxed);
    });
}

async fn take_down(host: &str, port: u16, user: &str) -> Result<(), String> {
    let dir = state_dir()?;
    let key_path = dir.join("server.key");
    if !key_path.is_file() {
        return Err(crate::text::tr("p.no_key").into());
    }
    let pem = fs::read(&key_path).map_err(|e| format!("key: {e}"))?;
    let pins = dir.join("host-keys.json");
    let addr = format!("{host}:{port}");
    let session = connect_key(&addr, user, &pem, &pins).await?;
    let out = exec(&session, "docker compose -f /opt/m1r4g3/docker-compose.yml down").await?;
    let _ = session.disconnect(russh::Disconnect::ByApplication, "", "").await;
    if out.exit.unwrap_or(1) != 0 {
        let err = out.stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("");
        if err.is_empty() {
            return Err(format!("{}: exit {:?}", crate::text::tr("p.remove_fail"), out.exit));
        }
        return Err(format!("{}: {err}", crate::text::tr("p.remove_fail")));
    }
    Ok(())
}

fn set_text(bus: &InstallBus, line: &str) {
    *bus.text.lock().unwrap_or_else(|p| p.into_inner()) = line.to_owned();
}

async fn install(job: Job, bus: &InstallBus) -> Result<Outcome, String> {
    let dir = state_dir()?;
    let key_path = dir.join("server.key");
    let pub_path = dir.join("server.pub");
    ensure_key(&key_path, &pub_path)?;
    let pub_line = fs::read_to_string(&pub_path).map_err(|e| format!("pubkey: {e}"))?;
    let pub_line = pub_line.trim().to_owned();
    let pem = fs::read(&key_path).map_err(|e| format!("key: {e}"))?;
    let pins = dir.join("host-keys.json");
    let addr = format!("{}:{}", job.host, job.port);

    let keyed = connect_key(&addr, &job.user, &pem, &pins).await;
    let session = match keyed {
        Ok(session) => session,
        Err(key_err) => {
            if job.password.is_empty() {
                return Err(key_err);
            }
            set_text(bus, crate::text::tr("p.key_on"));
            let password = job.password.clone();
            password_install_key(&addr, &job.user, &password, &pub_line, &pins).await?;
            drop(password);
            connect_key(&addr, &job.user, &pem, &pins).await?
        }
    };
    // Password is not used after the key is on the host.
    drop(job.password);

    // The client's key is on the box and it works — the install only gets
    // here after a successful key authentication. From now on the box
    // answers to that key and to nothing else: a password would be a second
    // way in that nobody needs.
    //
    // The step is best-effort. A box that will not take the change keeps its
    // password door, and the operator is told so — but the install stands.
    let mut warning = disable_password_auth(&session).await.err();

    set_text(bus, "docker");
    let remote = "/opt/m1r4g3";
    exec(&session, &format!("mkdir -p {remote}")).await?;
    upload(&session, COMPOSE.as_bytes(), &format!("{remote}/docker-compose.yml")).await?;
    let env = if job.env.is_empty() {
        format!("STACK={}\n", job.stack)
    } else {
        job.env
    };
    upload(&session, env.as_bytes(), &format!("{remote}/server.env")).await?;
    upload(&session, INSTALL_SH.as_bytes(), &format!("{remote}/install.sh")).await?;

    // The image goes up once. The second press of "install" must not push a
    // hundred megabytes again to change one port.
    if let Some(tar) = image_tar() {
        set_text(bus, crate::text::tr("p.image"));
        if !remote_has(&session, &format!("{remote}/image.tar")).await {
            let bytes = fs::read(&tar).map_err(|e| format!("image tar: {e}"))?;
            upload(&session, &bytes, &format!("{remote}/image.tar")).await?;
        }
    }

    if job.ask_faces {
        // Step one: the box is asked what it could publish. Nothing is
        // published yet — the person chooses, and the second press carries
        // the choice out.
        upload(&session, FACES_SH.as_bytes(), &format!("{remote}/faces.sh")).await?;
        set_text(bus, crate::text::tr("p.ports"));
        let out = tokio::time::timeout(
            INSTALL_TIMEOUT,
            exec(&session, &format!("sh {remote}/faces.sh")),
        )
        .await
        .map_err(|_| crate::text::tr("p.faces_timeout").to_owned())??;
        let _ = session.disconnect(russh::Disconnect::ByApplication, "", "").await;
        if out.exit.unwrap_or(1) != 0 {
            let err = out
                .stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("");
            return Err(if err.is_empty() {
                format!("{} (exit {:?})", crate::text::tr("p.no_proposal"), out.exit)
            } else {
                format!("{}: {err}", crate::text::tr("p.no_proposal"))
            });
        }
        return Ok(Outcome::Faces {
            faces: parse_faces(&out.stdout).map_err(|err| {
                // The box's own words about WHY it could not answer are more
                // useful than the client's summary of an empty answer.
                let last = out
                    .stderr
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("");
                if last.is_empty() {
                    err
                } else {
                    format!("{err}: {last}")
                }
            })?,
            warn: warning,
        });
    }

    // Step two: the box comes up with the faces the person chose.
    set_text(bus, crate::text::tr("p.container"));
    let out = tokio::time::timeout(
        INSTALL_TIMEOUT,
        exec(&session, &format!("sh {remote}/install.sh")),
    )
    .await
    .map_err(|_| crate::text::tr("p.install_timeout").to_owned())?
    .map_err(|err| with_warning(err, &warning))?;
    // The last word on the password door: the first attempt ran before the
    // container came up, and a box that was busy then may be free now.
    if let Err(reason) = disable_password_auth(&session).await {
        warning = warning.or(Some(reason));
    }
    let _ = session.disconnect(russh::Disconnect::ByApplication, "", "").await;
    let link = out
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("m1r4g3-link ").map(str::trim))
        .filter(|line| !line.is_empty())
        .ok_or_else(|| {
            let err = out.stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("");
            let text = if err.is_empty() {
                format!("{} (exit {:?})", crate::text::tr("p.no_link"), out.exit)
            } else {
                format!("{}: {err}", crate::text::tr("p.no_link"))
            };
            with_warning(text, &warning)
        })?;
    fs::write(dir.join("link.txt"), link).map_err(|e| format!("save link: {e}"))?;
    Ok(Outcome::Link {
        link: link.to_owned(),
        warn: warning,
    })
}

/// An error, with the warning attached when there is one.
///
/// The warning is not the reason the install failed, but the operator has to
/// see both: the box failed to come up, and it still has its password door.
fn with_warning(err: String, warn: &Option<String>) -> String {
    match warn {
        Some(warn) => format!("{err} ({warn})"),
        None => err,
    }
}

/// Close the password door on the box.
///
/// The client's own key is already on the box and already proven — the
/// install only reaches this point after a successful key authentication.
/// From here on the box answers to that key and to nothing else: a password
/// would be a second way in that nobody needs.
///
/// The step is best-effort. A box that will not take the change (no write
/// access, no `sshd` on the PATH, a `sudo` that wants a password) keeps its
/// password door, and the reason comes back to the operator as a warning
/// rather than as a failed install.
async fn disable_password_auth(
    session: &client::Handle<Handler>,
) -> Result<(), String> {
    let out = exec(session, &disable_password_script()).await?;
    if out.exit.unwrap_or(1) != 0 {
        let err = out
            .stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        return Err(if err.is_empty() {
            format!("{WARN_PASSWORD_DOOR}: exit {:?}", out.exit)
        } else {
            format!("{WARN_PASSWORD_DOOR}: {err}")
        });
    }
    if out.stdout.contains("m1r4g3-sshd-key-only") {
        return Ok(());
    }
    if let Some((_, reason)) = out.stdout.split_once("m1r4g3-sshd-skip") {
        return Err(format!("{WARN_PASSWORD_DOOR}: {}", reason.trim()));
    }
    Err(format!("{WARN_PASSWORD_DOOR}: бокс не ответил"))
}

/// The directives that make the box a key-only door.
///
/// `PasswordAuthentication no` is the point of the whole step: the client's
/// own key is on the box and works, so a password is a second way in that
/// nobody needs. `PermitRootLogin prohibit-password` keeps the root account
/// reachable BY KEY — the client talks to the box as root.
const SSHD_KEY_ONLY_DIRECTIVES: &str = "PasswordAuthentication no\n\
KbdInteractiveAuthentication no\n\
PermitRootLogin prohibit-password\n\
PubkeyAuthentication yes\n";

/// Where the drop-in goes when the box reads drop-ins.
const SSHD_DROPIN_PATH: &str = "/etc/ssh/sshd_config.d/90-mirage-key-only.conf";

/// The markers around the block when the box has no drop-in support.
///
/// The block goes to the TOP of `sshd_config`, because sshd takes the FIRST
/// value it sees for a keyword: a block at the end would lose against an
/// earlier `PasswordAuthentication yes`.
const SSHD_BLOCK_BEGIN: &str = "# BEGIN m1r4g3 key-only";
const SSHD_BLOCK_END: &str = "# END m1r4g3 key-only";

/// What the operator is told when the box keeps its password door.
const WARN_PASSWORD_DOOR: &str = "парольный вход на боксе не отключён";

/// The shell that turns the box into a key-only door.
///
/// The step is built so that running it twice is the same as running it
/// once: the drop-in is written from scratch, and the marked block is
/// removed before it is written again.
///
/// Two ways to write the directives, and the choice between them matters:
/// sshd takes the FIRST value it sees for a keyword, so a drop-in only wins
/// when the `Include` that pulls it in comes before every directive of ours.
/// When it does not, the block goes to the top of `sshd_config` instead —
/// there it always wins. Silently losing the race would leave the password
/// door open while the step reports success.
///
/// `sshd -t` has to pass BEFORE sshd is reloaded — a broken config never
/// reaches the running daemon, and the box keeps the config it had. The
/// reload is deliberately not a restart: a restart would drop the very
/// session this script runs in.
fn disable_password_script() -> String {
    let dropin = format!(
        "# Installed by the m1r4g3 client after its own key was verified.\n\
         # The key is the only way in now.\n\
         {SSHD_KEY_ONLY_DIRECTIVES}"
    );
    format!(
        r#"#!/bin/sh
set -eu
if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
else
  if ! sudo -n true 2>/dev/null; then
    echo "m1r4g3-sshd-skip sudo needs a password"
    exit 0
  fi
  SUDO="sudo -n"
fi
SSHD=$(command -v sshd || echo /usr/sbin/sshd)
CONF=/etc/ssh/sshd_config
DROPIN={dropin_path}
TMP=$(mktemp /tmp/m1r4g3-key-only.XXXXXX)
trap 'rm -f "$TMP"' EXIT
cat > "$TMP" <<'M1R4G3_EOF'
{dropin}M1R4G3_EOF
include_at=$(grep -in -m1 '^[[:space:]]*Include[[:space:]].*sshd_config\.d' "$CONF" | cut -d: -f1)
ours_at=$(grep -in -m1 -E '^[[:space:]]*(PasswordAuthentication|KbdInteractiveAuthentication|PermitRootLogin)[[:space:]]' "$CONF" | cut -d: -f1)
if [ -n "$include_at" ] && {{ [ -z "$ours_at" ] || [ "$include_at" -lt "$ours_at" ]; }}; then
  mkdir -p "$(dirname "$DROPIN")"
  $SUDO cp -p "$CONF" "$CONF.m1r4g3.bak"
  $SUDO cp "$TMP" "$DROPIN"
else
  $SUDO cp -p "$CONF" "$CONF.m1r4g3.bak"
  $SUDO sed -i '/^{begin}$/,/^{end}$/d' "$CONF"
  MERGED=$(mktemp /tmp/m1r4g3-sshd-config.XXXXXX)
  {{ printf '%s\n' '{begin}'; cat "$TMP"; printf '%s\n' '{end}'; cat "$CONF"; }} > "$MERGED"
  $SUDO cp "$MERGED" "$CONF"
  rm -f "$MERGED"
fi
if ! $SUDO "$SSHD" -t; then
  echo "sshd -t refused the config" >&2
  exit 1
fi
if $SUDO systemctl reload ssh 2>/dev/null; then
  :
elif $SUDO systemctl reload sshd 2>/dev/null; then
  :
elif $SUDO service ssh reload 2>/dev/null; then
  :
else
  PID=$(cat /run/sshd.pid 2>/dev/null || true)
  if [ -n "$PID" ] && $SUDO kill -HUP "$PID" 2>/dev/null; then
    :
  else
    echo "could not reload sshd" >&2
    exit 1
  fi
fi
echo "m1r4g3-sshd-key-only"
"#,
        dropin_path = SSHD_DROPIN_PATH,
        dropin = dropin,
        begin = SSHD_BLOCK_BEGIN,
        end = SSHD_BLOCK_END,
    )
}

/// Read the proposal out of the box's answer.
///
/// The proposal is printed between two markers, because the box says other
/// things on the same stream — `docker load` writes to stdout too, and a
/// proposal that swallowed a docker banner would be a proposal nobody could
/// trust.
fn parse_faces(stdout: &str) -> Result<Vec<ProposedFace>, String> {
    let mut inside = false;
    let mut body = String::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line == FACES_BEGIN {
            inside = true;
            continue;
        }
        if line == FACES_END {
            break;
        }
        if inside {
            body.push_str(line);
            body.push('\n');
        }
    }
    if body.trim().is_empty() {
        return Err(crate::text::tr("p.no_faces_doc").into());
    }
    parse_proposal(&body)
}

/// Read the proposal file the box writes.
fn parse_proposal(text: &str) -> Result<Vec<ProposedFace>, String> {
    let proposal: FaceProposal =
        toml::from_str(text).map_err(|e| format!("{}: {e}", crate::text::tr("p.faces_bad")))?;
    if proposal.faces.is_empty() {
        return Err(crate::text::tr("p.no_face").into());
    }
    Ok(proposal.faces)
}

/// Whether the box already has that file.
async fn remote_has(session: &client::Handle<Handler>, path: &str) -> bool {
    match exec(session, &format!("test -f {path}")).await {
        Ok(out) => out.exit == Some(0),
        Err(_) => false,
    }
}

fn state_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("exe: {e}"))?;
    let dir = exe.parent().ok_or("exe dir")?.join("standalone");
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    Ok(dir)
}

fn image_tar() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join("m1r4g3-server.tar");
    path.is_file().then_some(path)
}

fn ensure_key(priv_path: &Path, pub_path: &Path) -> Result<(), String> {
    if priv_path.is_file() && pub_path.is_file() {
        return Ok(());
    }
    let mut key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).map_err(|e| e.to_string())?;
    key.set_comment("m1r4g3-standalone");
    let priv_openssh = key.to_openssh(LineEnding::LF).map_err(|e| e.to_string())?;
    let pub_openssh = key.public_key().to_openssh().map_err(|e| e.to_string())?;
    fs::write(priv_path, priv_openssh.as_bytes()).map_err(|e| e.to_string())?;
    fs::write(pub_path, pub_openssh.as_bytes()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(priv_path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

struct ExecOut {
    exit: Option<u32>,
    stdout: String,
    stderr: String,
}

struct Handler {
    addr: String,
    pins: PathBuf,
    mismatch: Arc<Mutex<Option<String>>>,
}

#[async_trait::async_trait]
impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, server_public_key: &PublicKey) -> Result<bool, Self::Error> {
        let blob = match server_public_key.to_bytes() {
            Ok(blob) => blob,
            Err(_) => return Ok(false),
        };
        let fp = hex::encode(sha2::Sha256::digest(&blob));
        match pin(&self.pins, &self.addr, &fp) {
            Ok(true) => Ok(true),
            Ok(false) => {
                *self.mismatch.lock().unwrap_or_else(|p| p.into_inner()) = Some(fp);
                Ok(false)
            }
            Err(_) => Ok(false),
        }
    }
}

/// `true` when the pin matches or was just stored. `false` on mismatch.
fn pin(path: &Path, addr: &str, fp: &str) -> Result<bool, String> {
    let mut map: std::collections::HashMap<String, String> = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => std::collections::HashMap::new(),
    };
    match map.get(addr) {
        Some(have) if have == fp => Ok(true),
        Some(_) => Ok(false),
        None => {
            map.insert(addr.to_string(), fp.to_string());
            let text = serde_json::to_string(&map).map_err(|e| e.to_string())?;
            fs::write(path, text).map_err(|e| e.to_string())?;
            Ok(true)
        }
    }
}

async fn connect_key(
    addr: &str,
    user: &str,
    pem: &[u8],
    pins: &Path,
) -> Result<client::Handle<Handler>, String> {
    let text = std::str::from_utf8(pem).map_err(|_| "key encoding".to_owned())?;
    let key = russh::keys::decode_secret_key(text, None).map_err(|e| format!("key: {e}"))?;
    let alg = PrivateKeyWithHashAlg::new(Arc::new(key), None).map_err(|e| format!("key: {e}"))?;
    let mut session = connect(addr, pins).await?;
    let ok = session
        .authenticate_publickey(user, alg)
        .await
        .map_err(|e| format!("ssh: {e}"))?;
    if !ok {
        return Err(crate::text::tr("p.ssh_key").into());
    }
    Ok(session)
}

async fn password_install_key(
    addr: &str,
    user: &str,
    password: &str,
    pub_line: &str,
    pins: &Path,
) -> Result<(), String> {
    let mut session = connect(addr, pins).await?;
    let ok = session
        .authenticate_password(user, password)
        .await
        .map_err(|e| format!("ssh: {e}"))?;
    if !ok {
        return Err(crate::text::tr("p.ssh_pass").into());
    }
    let cmd = format!(
        "mkdir -p ~/.ssh && chmod 700 ~/.ssh && (grep -qF {q} ~/.ssh/authorized_keys 2>/dev/null || echo {q} >> ~/.ssh/authorized_keys) && chmod 600 ~/.ssh/authorized_keys",
        q = sh_quote(pub_line)
    );
    let out = exec(&session, &cmd).await?;
    let _ = session.disconnect(russh::Disconnect::ByApplication, "", "").await;
    if out.exit.unwrap_or(1) != 0 {
        let err = out.stderr.trim();
        return Err(if err.is_empty() {
            crate::text::tr("p.key_write").into()
        } else {
            format!("{}: {err}", crate::text::tr("p.key_write"))
        });
    }
    Ok(())
}

async fn connect(addr: &str, pins: &Path) -> Result<client::Handle<Handler>, String> {
    let mismatch = Arc::new(Mutex::new(None));
    let handler = Handler {
        addr: addr.to_owned(),
        pins: pins.to_path_buf(),
        mismatch: Arc::clone(&mismatch),
    };
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(120)),
        ..Default::default()
    });
    match tokio::time::timeout(CONNECT_TIMEOUT, client::connect(config, addr, handler)).await {
        Err(_) => Err(format!("ssh {addr}: {}", crate::text::tr("p.ssh_timeout"))),
        Ok(Err(err)) => {
            if mismatch.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
                Err(format!("ssh {addr}: {}", crate::text::tr("p.ssh_host")))
            } else {
                Err(format!("ssh {addr}: {err}"))
            }
        }
        Ok(Ok(session)) => Ok(session),
    }
}

async fn exec(session: &client::Handle<Handler>, command: &str) -> Result<ExecOut, String> {
    let mut channel = session.channel_open_session().await.map_err(|e| format!("ssh: {e}"))?;
    channel
        .exec(true, format!("sh -lc {}", sh_quote(command)))
        .await
        .map_err(|e| format!("ssh: {e}"))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit = None;
    while let Some(msg) = channel.wait().await {
        use russh::ChannelMsg;
        match msg {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, ext: 1 } => stderr.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(ExecOut {
        exit,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

async fn upload(session: &client::Handle<Handler>, bytes: &[u8], remote: &str) -> Result<(), String> {
    let channel = session.channel_open_session().await.map_err(|e| format!("sftp: {e}"))?;
    channel.request_subsystem(true, "sftp").await.map_err(|e| format!("sftp: {e}"))?;
    let sftp = SftpSession::new(channel.into_stream()).await.map_err(|e| format!("sftp: {e}"))?;
    let tmp = format!("{remote}.new");
    {
        let mut file = sftp.create(&tmp).await.map_err(|e| format!("sftp create: {e}"))?;
        const CHUNK: usize = 32 * 1024;
        let mut offset = 0;
        while offset < bytes.len() {
            let end = (offset + CHUNK).min(bytes.len());
            file.write_all(&bytes[offset..end]).await.map_err(|e| format!("sftp write: {e}"))?;
            offset = end;
        }
        file.flush().await.map_err(|e| format!("sftp flush: {e}"))?;
        file.shutdown().await.map_err(|e| format!("sftp close: {e}"))?;
    }
    let meta = sftp.metadata(&tmp).await.map_err(|e| format!("sftp stat: {e}"))?;
    if meta.size.unwrap_or(0) != bytes.len() as u64 {
        let _ = sftp.remove_file(&tmp).await;
        return Err(format!("sftp {remote}: {}", crate::text::tr("p.sftp_size")));
    }
    // Permissions only. A default FileAttributes sets size to 0 and truncates.
    let attrs = russh_sftp::protocol::FileAttributes {
        size: None,
        uid: None,
        user: None,
        gid: None,
        group: None,
        permissions: Some(0o644),
        atime: None,
        mtime: None,
    };
    sftp.set_metadata(&tmp, attrs).await.map_err(|e| format!("sftp chmod: {e}"))?;
    let _ = sftp.remove_file(remote).await;
    sftp.rename(&tmp, remote).await.map_err(|e| format!("sftp rename: {e}"))?;
    Ok(())
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

const COMPOSE: &str = concat!(
    "services:\n",
    "  mirage:\n",
    "    image: ",
    "m1r4g3-server:1\n",
    "    container_name: m1r4g3\n",
    "    restart: unless-stopped\n",
    "    network_mode: host\n",
    "    env_file:\n",
    "      - server.env\n",
    "    volumes:\n",
    "      - m1r4g3-data:/data\n",
    // The server reads its faces from the FILE, and the file lives next to
    // this compose file. `env_file` only injects the variables into the
    // process environment — the container never sees the file itself, and
    // docker would create a directory in its place. So the file is mounted
    // read-only at the path the server expects.
    "      - /opt/m1r4g3/server.env:/opt/m1r4g3/server.env:ro\n",
    "volumes:\n",
    "  m1r4g3-data:\n",
);

/// Step one: ask the box what it could publish.
///
/// The proposal is printed between two markers, because the box says other
/// things on the same stream. `docker load` writes to stdout as well, and a
/// proposal that swallowed a docker banner would be a proposal nobody could
/// trust.
const FACES_SH: &str = r#"#!/bin/sh
set -eu
if [ "$(id -u)" -eq 0 ]; then SUDO=""; else SUDO="sudo"; fi
if ! command -v docker >/dev/null 2>&1; then
  if command -v apt-get >/dev/null 2>&1; then
    export DEBIAN_FRONTEND=noninteractive
    $SUDO apt-get update
    $SUDO apt-get install -y docker.io
    $SUDO apt-get install -y docker-compose-v2 || true
  elif command -v dnf >/dev/null 2>&1; then
    $SUDO dnf install -y docker
  else
    echo "no docker and no apt-get or dnf" >&2
    exit 20
  fi
  if command -v systemctl >/dev/null 2>&1; then
    $SUDO systemctl enable --now docker || true
  fi
fi
cd /opt/m1r4g3
# The compose file names its env file, and compose refuses to run without
# it — even for a one-off run that reads nothing from it.
[ -f server.env ] || printf 'STACK=auto\n' > server.env
if [ -f image.tar ]; then
  $SUDO docker load -i image.tar >&2
fi
echo "m1r4g3-faces-begin"
if $SUDO docker compose version >/dev/null 2>&1; then
  $SUDO docker compose run --rm -T --entrypoint /bin/sh mirage -c 'm1r4g3-server propose --dir /data --format toml'
else
  $SUDO docker-compose run --rm -T --entrypoint /bin/sh mirage -c 'm1r4g3-server propose --dir /data --format toml'
fi
echo "m1r4g3-faces-end"
"#;

/// Step two: bring the box up with the faces the person chose, and read the
/// one line the customer needs.
///
/// The contract is unchanged: the same image, the same compose file, the same
/// volume, and the same `m1r4g3-link <LINK>` line on stdout.
const INSTALL_SH: &str = r#"#!/bin/sh
set -eu
if [ "$(id -u)" -eq 0 ]; then SUDO=""; else SUDO="sudo"; fi
if ! command -v docker >/dev/null 2>&1; then
  echo "no docker and no apt-get or dnf" >&2
  exit 20
fi
cd /opt/m1r4g3
# The compose file names server.env twice: as env_file and as a read-only
# bind mount. If the file does not exist, docker would create a DIRECTORY
# at that path and compose would refuse to read it as an env file. So the
# file is created before the first container starts.
[ -f server.env ] || printf 'STACK=auto\n' > server.env
if [ -f image.tar ]; then
  $SUDO docker load -i image.tar
fi
if $SUDO docker compose version >/dev/null 2>&1; then
  $SUDO docker compose up -d
  LINK="$($SUDO docker compose exec -T mirage cat /data/client.link 2>/dev/null | head -n 1 | tr -d '\r' || true)"
else
  $SUDO docker-compose up -d
  LINK="$($SUDO docker-compose exec -T mirage cat /data/client.link 2>/dev/null | head -n 1 | tr -d '\r' || true)"
fi
if [ -z "$LINK" ]; then
  echo "client.link is empty; image must be m1r4g3-server:1 and write /data/client.link" >&2
  # The box's own words — a refused port, a bad mask — are in the container's
  # log, and the operator is the one who has to read them.
  $SUDO docker compose logs --tail 40 >&2 || true
  $SUDO docker ps >&2 || true
  exit 21
fi
printf 'm1r4g3-link %s\n' "$LINK"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The proposal exactly as the box writes it — the file
    /// `m1r4g3-server propose` puts in the box's data directory.
    const SAMPLE: &str = r#"[[face]]
port = 443
transport = "tcp"
mask = '{"type":"tls_real","sni":"srv7451.coreserve.cloud","alpn":["h2"]}'
why = "443/TCP — главный HTTPS-порт."

[[face]]
port = 443
transport = "udp"
mask = '{"type":"quic","alpn":["h3"],"bind_addr":"0.0.0.0:443"}'
why = "443/UDP — там обычно весит QUIC."
"#;

    #[test]
    fn the_proposal_the_box_writes_is_read() {
        let faces = parse_proposal(SAMPLE).expect("the proposal reads");
        assert_eq!(faces.len(), 2);
        assert_eq!(faces[0].port, 443);
        assert_eq!(faces[0].transport, "tcp");
        assert!(faces[0].mask.contains("tls_real"));
        assert_eq!(faces[1].transport, "udp");
        assert!(faces[1].mask.contains("\"bind_addr\":\"0.0.0.0:443\""));
    }

    #[test]
    fn the_proposal_is_read_out_of_the_markers() {
        let stdout = format!(
            "Loaded image: m1r4g3-server:1\n{FACES_BEGIN}\n{SAMPLE}{FACES_END}\nsomething else\n"
        );
        let faces = parse_faces(&stdout).expect("the proposal reads");
        assert_eq!(faces.len(), 2);
    }

    #[test]
    fn a_stream_without_a_proposal_is_refused() {
        let err = parse_faces("Loaded image: m1r4g3-server:1\n").expect_err("nothing to read");
        assert!(err.contains("не прислал") || err.contains("did not send"), "{err}");
    }

    #[test]
    fn the_key_only_door_has_exactly_the_four_directives() {
        let script = disable_password_script();
        for directive in [
            "PasswordAuthentication no",
            "KbdInteractiveAuthentication no",
            "PermitRootLogin prohibit-password",
            "PubkeyAuthentication yes",
        ] {
            assert!(script.contains(directive), "{directive} is missing");
        }
    }

    #[test]
    fn the_config_is_checked_before_sshd_is_reloaded() {
        let script = disable_password_script();
        let check = script.find("\"$SSHD\" -t").expect("the config is checked");
        let reload = script.find("reload").expect("sshd is reloaded");
        assert!(check < reload, "the check must come before the reload");
        assert!(
            !script.contains("restart"),
            "a restart would drop the session the script runs in"
        );
    }

    #[test]
    fn the_reload_has_a_way_that_works_without_systemd() {
        // A HUP on the running master makes it re-read its config and keeps
        // the sessions that are already open — a restart would not.
        let script = disable_password_script();
        assert!(script.contains("kill -HUP"), "the HUP fallback is there");
        assert!(script.contains("/run/sshd.pid"), "the master pid is found");
    }

    #[test]
    fn the_step_never_asks_for_a_password() {
        let script = disable_password_script();
        assert!(script.contains("sudo -n"), "sudo must be non-interactive");
        assert!(
            !script.contains("sudo -S"),
            "sudo must not read a password from the stream"
        );
    }

    #[test]
    fn the_marked_block_is_removed_before_it_is_written_again() {
        let script = disable_password_script();
        assert!(script.contains(SSHD_BLOCK_BEGIN));
        assert!(script.contains(SSHD_BLOCK_END));
        assert!(
            script.contains(&format!("/^{}$/,/^{}$/d", SSHD_BLOCK_BEGIN, SSHD_BLOCK_END)),
            "the old block must be deleted before the new one is written"
        );
    }

    #[test]
    fn the_block_goes_before_the_old_config_because_sshd_takes_the_first_value() {
        let script = disable_password_script();
        let begin = script
            .find(&format!("printf '%s\\n' '{SSHD_BLOCK_BEGIN}'"))
            .expect("the block is written");
        let old = script.find("cat \"$CONF\"").expect("the old config is kept");
        assert!(begin < old, "the block must be written before the old config");
    }

    #[test]
    fn the_dropin_goes_where_sshd_reads_it() {
        let script = disable_password_script();
        assert!(script.contains(SSHD_DROPIN_PATH));
        assert!(script.contains("sshd_config\\.d"), "the drop-in dir is probed");
    }

    #[test]
    fn the_dropin_is_only_used_when_it_wins_the_first_value_race() {
        // sshd takes the first value it sees for a keyword, so a drop-in
        // that is included AFTER another `PasswordAuthentication` would lose
        // and leave the password door open. The script has to compare the
        // two positions and fall back to the top of the config.
        let script = disable_password_script();
        assert!(script.contains("include_at"), "the Include position is read");
        assert!(script.contains("ours_at"), "our own position is read");
        assert!(
            script.contains("[ \"$include_at\" -lt \"$ours_at\" ]"),
            "the Include must come before our directives"
        );
    }

    #[test]
    fn a_box_that_cannot_take_the_change_says_so() {
        let script = disable_password_script();
        assert!(script.contains("m1r4g3-sshd-key-only"));
        assert!(script.contains("m1r4g3-sshd-skip"));
    }
}
