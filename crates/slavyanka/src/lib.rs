//! mirage-slavyanka — operator-triggered reverse-tunnel for OUR trusted box.
//!
//! Spawned by `mirage-sentry` when the monitor pushes a `DebugTunnelOpen`
//! WS frame. Everything that defines the tunnel target (jump host, SSH
//! identity, SOCKS5 hop) is **compiled in** — slavyanka takes no config
//! file. The only runtime input is the reverse port the LM forwards back
//! to us, passed as a CLI arg.
//!
//! Wire:
//!
//! ```text
//! [tester1]
//!   sentry -> spawn(mirage-slavyanka --reverse-port N)
//!   slavyanka:
//!     - bind 127.0.0.1:31337 (cmd.exe pipe wrapper)
//!     - ssh out via mirage SOCKS5 (127.0.0.1:10800)
//!       -> [DEV_BOX_SSH_HOST]:22, user [DEV_BOX_SSH_USER]
//!       -R N:127.0.0.1:31337
//! [operator on dev-box]:  nc localhost N  ->  cmd.exe on tester1
//! ```

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use russh::client::{self, Handle};
use russh::keys::PrivateKeyWithHashAlg;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio_socks::tcp::Socks5Stream;
use tracing::{debug, error, info, warn};

// ── Compile-time configuration (no slavyanka.toml on disk) ────────────────────

/// Dev-box SSH server we dial OUT to. Operator lands here too:
///   ssh -p <reverse_port> -i <op-key> <DEV_BOX_SSH_USER>@<DEV_BOX_SSH_HOST>
/// (or via ProxyJump if dev-box behind NAT; the reverse forward is the
///  payload, the operator's transport to dev-box is their own problem.)
pub const DEV_BOX_SSH_HOST: &str = "192.168.1.3:22";
pub const DEV_BOX_SSH_USER: &str = "VA PC";

/// Local shell-wrapper port. slavyanka binds this on 127.0.0.1 and pipes
/// every incoming connection to a fresh `cmd.exe` (Windows) / `/bin/sh -i`
/// (other). The reverse SSH forward targets THIS port — there's no
/// external SSH server on the tester box.
pub const SHELL_WRAPPER_PORT: u16 = 31337;

/// SOCKS5 proxy attempted first (Mirage client-cli's local listener). If
/// the dial fails (no cli running, port blocked, etc.) slavyanka falls
/// back to direct TCP — useful on the same LAN as the dev-box.
pub const SOCKS5_PROXY: &str = "127.0.0.1:10800";

/// Embedded ed25519 priv key for `DEV_BOX_SSH_USER@DEV_BOX_SSH_HOST`.
/// Generated once at build-time; the matching pub is in dev-box's
/// `~/.ssh/authorized_keys`. Trusted-box mode — no passphrase.
const EMBEDDED_SSH_PRIV: &str =
    include_str!("../../../../env/local/slavyanka-tester1-ed25519");

// ── Reconnect backoff bounds ──────────────────────────────────────────────────

const INITIAL_BACKOFF_SECS: u64 = 1;
const MAX_BACKOFF_SECS: u64 = 60;

// ── Russh client handler — bridges forwarded-tcpip channels to wrapper ────────

struct ShellBridgeClient {
    shell_wrapper_port: u16,
}

impl client::Handler for ShellBridgeClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
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
/// socket write half. stderr also forwarded (interleaved with stdout
/// under a mutex). No PTY — works for non-interactive commands.
pub async fn run_shell_wrapper(port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("shell wrapper bind 127.0.0.1:{port}"))?;
    info!(port, "[slavyanka] shell wrapper listening");
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

    let stdin_task = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut sock_read, &mut stdin).await;
        let _ = stdin.shutdown().await;
    });
    let sw_out = Arc::clone(&sock_write);
    let stdout_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if sw_out.lock().await.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    });
    let sw_err = Arc::clone(&sock_write);
    let stderr_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if sw_err.lock().await.write_all(&buf[..n]).await.is_err() {
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

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}

/// Dial dev-box SSH server. Try SOCKS5 first (Mirage hop); fall back to
/// direct TCP on SOCKS5 failure (LAN reachability case).
async fn dial_dev_box() -> Result<Box<dyn Stream>> {
    let (host, port) = split_host_port(DEV_BOX_SSH_HOST)?;
    match Socks5Stream::connect(SOCKS5_PROXY, (host.as_str(), port)).await {
        Ok(s) => {
            info!(via = SOCKS5_PROXY, target = DEV_BOX_SSH_HOST, "[slavyanka] dialed via SOCKS5");
            Ok(Box::new(s))
        }
        Err(e) => {
            debug!(error = %e, "[slavyanka] SOCKS5 dial failed, falling back to direct");
            let s = TcpStream::connect(DEV_BOX_SSH_HOST)
                .await
                .with_context(|| format!("direct TCP dial {}", DEV_BOX_SSH_HOST))?;
            info!(target = DEV_BOX_SSH_HOST, "[slavyanka] dialed directly");
            Ok(Box::new(s))
        }
    }
}

fn split_host_port(host_port: &str) -> Result<(String, u16)> {
    let (h, p) = host_port
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("host:port malformed: {host_port}"))?;
    let port: u16 = p.parse().with_context(|| format!("parse port from `{host_port}`"))?;
    Ok((h.to_string(), port))
}

// ── One SSH session ───────────────────────────────────────────────────────────

async fn run_once(reverse_port: u16) -> Result<()> {
    let key = russh::keys::decode_secret_key(EMBEDDED_SSH_PRIV, None)
        .context("decode embedded SSH private key")?;

    let stream = dial_dev_box().await?;

    let ssh_cfg = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });

    let handler = ShellBridgeClient {
        shell_wrapper_port: SHELL_WRAPPER_PORT,
    };
    let mut session = client::connect_stream(ssh_cfg, stream, handler)
        .await
        .context("SSH handshake (KEX) failed")?;

    let authed = session
        .authenticate_publickey(
            DEV_BOX_SSH_USER,
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        )
        .await
        .context("SSH publickey auth")?;
    if !authed.success() {
        return Err(anyhow!(
            "SSH publickey auth rejected by `{}@{}`",
            DEV_BOX_SSH_USER, DEV_BOX_SSH_HOST
        ));
    }
    info!(user = DEV_BOX_SSH_USER, host = DEV_BOX_SSH_HOST, "[slavyanka] SSH authenticated");

    let _bound = session
        .tcpip_forward("127.0.0.1", reverse_port as u32)
        .await
        .context("server rejected tcpip-forward request")?;
    info!(reverse_port, "[slavyanka] tcpip-forward active");
    info!(
        "[slavyanka] operator: nc localhost {} (from {}@{})",
        reverse_port, DEV_BOX_SSH_USER, DEV_BOX_SSH_HOST
    );

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

// ── Public entry point ────────────────────────────────────────────────────────

/// Run slavyanka: spawn the shell wrapper, then loop SSH reverse-forward
/// with exponential backoff. `reverse_port` is the port on dev-box that
/// will end up forwarded to tester1's shell wrapper.
pub async fn run(reverse_port: u16) -> Result<()> {
    tokio::spawn(async move {
        if let Err(e) = run_shell_wrapper(SHELL_WRAPPER_PORT).await {
            error!(error = %e, "[slavyanka] shell wrapper exited");
        }
    });

    let initial = Duration::from_secs(INITIAL_BACKOFF_SECS.max(1));
    let cap = Duration::from_secs(MAX_BACKOFF_SECS.max(INITIAL_BACKOFF_SECS).max(1));
    let mut delay = initial;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    loop {
        let attempt = run_once(reverse_port);
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
