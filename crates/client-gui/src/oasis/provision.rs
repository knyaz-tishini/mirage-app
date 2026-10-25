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

pub struct InstallBus {
    pub running: std::sync::atomic::AtomicBool,
    pub text: Mutex<String>,
    pub link: Mutex<Option<String>>,
}

impl InstallBus {
    pub fn new() -> Self {
        Self {
            running: std::sync::atomic::AtomicBool::new(false),
            text: Mutex::new(String::new()),
            link: Mutex::new(None),
        }
    }
}

pub struct Job {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub stack: String,
}

pub fn start(job: Job, bus: Arc<InstallBus>) {
    use std::sync::atomic::Ordering;
    if bus.running.swap(true, Ordering::Relaxed) {
        return;
    }
    set_text(&bus, "соединение");
    std::thread::spawn(move || {
        let outcome = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(install(job, &bus)),
            Err(err) => Err(format!("runtime: {err}")),
        };
        match outcome {
            Ok(link) => {
                *bus.link.lock().unwrap_or_else(|p| p.into_inner()) = Some(link);
                set_text(&bus, "сервер поднят");
            }
            Err(err) => set_text(&bus, &err),
        }
        bus.running.store(false, Ordering::Relaxed);
    });
}

fn set_text(bus: &InstallBus, line: &str) {
    *bus.text.lock().unwrap_or_else(|p| p.into_inner()) = line.to_owned();
}

async fn install(job: Job, bus: &InstallBus) -> Result<String, String> {
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
            set_text(bus, "ключ на сервере");
            let password = job.password.clone();
            password_install_key(&addr, &job.user, &password, &pub_line, &pins).await?;
            drop(password);
            connect_key(&addr, &job.user, &pem, &pins).await?
        }
    };
    // Password is not used after the key is on the host.
    drop(job.password);

    set_text(bus, "docker");
    let remote = "/opt/m1r4g3";
    exec(&session, &format!("mkdir -p {remote}")).await?;
    upload(&session, COMPOSE.as_bytes(), &format!("{remote}/docker-compose.yml")).await?;
    let env = format!("STACK={}\n", job.stack);
    upload(&session, env.as_bytes(), &format!("{remote}/server.env")).await?;
    upload(&session, INSTALL_SH.as_bytes(), &format!("{remote}/install.sh")).await?;
    if let Some(tar) = image_tar() {
        set_text(bus, "образ");
        let bytes = fs::read(&tar).map_err(|e| format!("image tar: {e}"))?;
        upload(&session, &bytes, &format!("{remote}/image.tar")).await?;
    }
    set_text(bus, "контейнер");
    let out = tokio::time::timeout(
        INSTALL_TIMEOUT,
        exec(&session, &format!("sh {remote}/install.sh")),
    )
    .await
    .map_err(|_| "установка дольше 15 минут".to_owned())??;
    let _ = session.disconnect(russh::Disconnect::ByApplication, "", "").await;
    let link = out
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("m1r4g3-link ").map(str::trim))
        .filter(|line| !line.is_empty())
        .ok_or_else(|| {
            let err = out.stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("");
            if err.is_empty() {
                format!("контейнер не отдал ссылку (exit {:?})", out.exit)
            } else {
                format!("контейнер не отдал ссылку: {err}")
            }
        })?;
    fs::write(dir.join("link.txt"), link).map_err(|e| format!("save link: {e}"))?;
    Ok(link.to_owned())
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
        return Err("ssh: ключ не принят".into());
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
        return Err("ssh: пароль не принят".into());
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
            "не удалось записать ключ".into()
        } else {
            format!("не удалось записать ключ: {err}")
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
        Err(_) => Err(format!("ssh {addr}: таймаут")),
        Ok(Err(err)) => {
            if mismatch.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
                Err(format!("ssh {addr}: ключ хоста сменился"))
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
        return Err(format!("sftp: размер {remote} не сошёлся"));
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
    "volumes:\n",
    "  m1r4g3-data:\n",
);

const INSTALL_SH: &str = r#"#!/bin/sh
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
  $SUDO docker ps >&2 || true
  exit 21
fi
printf 'm1r4g3-link %s\n' "$LINK"
"#;
