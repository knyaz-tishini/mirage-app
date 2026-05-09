//! mirage-debug-shell — opens a reverse SSH tunnel from the client back to the
//! monitor host so an operator can `ssh -p <reverse_port> localhost` from the
//! monitor box and land on this machine.
//!
//! Lifetime is bounded by `ttl_secs`; SIGTERM/Ctrl-C tears the tunnel down
//! immediately. Sentinel spawns this binary on `WsServerFrame::DebugTunnelOpen`
//! and SIGTERMs it on `WsServerFrame::DebugTunnelClose`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use serde::Deserialize;
use tokio::process::Command;

#[derive(Parser)]
#[command(
    name = "mirage-debug-shell",
    version,
    about = "Reverse SSH tunnel — operator-triggered debug shell"
)]
struct Args {
    /// Path to JSON config file (DebugTunnelOpen payload).
    #[arg(long)]
    config: PathBuf,
}

#[derive(Debug, Deserialize)]
struct DebugConfig {
    monitor_ssh_host: String,
    #[serde(default = "default_ssh_port")]
    monitor_ssh_port: u16,
    monitor_ssh_user: String,
    ssh_priv_key_pem: String,
    #[serde(default = "default_reverse_port")]
    reverse_port: u16,
    #[serde(default = "default_local_ssh_port")]
    local_ssh_port: u16,
    #[serde(default = "default_ttl_secs")]
    ttl_secs: u64,
}

fn default_ssh_port() -> u16 { 22 }
fn default_reverse_port() -> u16 { 22500 }
fn default_local_ssh_port() -> u16 { 22 }
fn default_ttl_secs() -> u64 { 3600 }

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    // Standard L0 panic catcher — log + exit(1).
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".into());
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".into()
        };
        tracing::error!(location=%location, message=%msg, "panic — process exiting");
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::process::exit(1);
    }));

    // Memory guard — exit(0) on fatal so parent can respawn.
    let _memory_guard = MemoryGuard::spawn(
        MemoryGuardConfig::default(),
        Box::new(|| {
            tracing::warn!("[debug-shell] memory guard fatal — exiting");
            std::process::exit(0);
        }),
    );

    let args = Args::parse();
    tracing::info!("[debug-shell] loading config {:?}", args.config);
    let raw = tokio::fs::read_to_string(&args.config)
        .await
        .with_context(|| format!("read debug-shell config {:?}", args.config))?;
    let cfg: DebugConfig = serde_json::from_str(&raw).context("parse debug-shell JSON")?;

    let key_path = write_priv_key(&cfg.ssh_priv_key_pem).await?;
    tracing::info!("[debug-shell] private key written to {:?}", key_path);

    let forward_spec = format!(
        "{rev}:127.0.0.1:{loc}",
        rev = cfg.reverse_port,
        loc = cfg.local_ssh_port,
    );
    let user_host = format!("{}@{}", cfg.monitor_ssh_user, cfg.monitor_ssh_host);

    tracing::info!(
        "[debug-shell] ssh -N -R {} -p {} {} (ttl {}s)",
        forward_spec, cfg.monitor_ssh_port, user_host, cfg.ttl_secs
    );

    let mut child = Command::new("ssh")
        .arg("-N")
        .arg("-i")
        .arg(&key_path)
        .arg("-R")
        .arg(&forward_spec)
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null")
        .arg("-o")
        .arg("ServerAliveInterval=30")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-p")
        .arg(cfg.monitor_ssh_port.to_string())
        .arg(&user_host)
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn ssh")?;

    let child_pid = child.id();
    tracing::info!("[debug-shell] ssh child pid={:?}", child_pid);

    let ttl = Duration::from_secs(cfg.ttl_secs.max(1));
    let result = tokio::select! {
        st = child.wait() => {
            match st {
                Ok(s) => {
                    tracing::warn!("[debug-shell] ssh exited unexpectedly: {:?}", s);
                    Ok(())
                }
                Err(e) => {
                    tracing::error!("[debug-shell] ssh wait error: {}", e);
                    Err(anyhow::anyhow!(e))
                }
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("[debug-shell] Ctrl-C / SIGTERM received");
            terminate_child(&mut child).await;
            Ok(())
        }
        _ = tokio::time::sleep(ttl) => {
            tracing::info!("[debug-shell] TTL {}s elapsed — closing tunnel", cfg.ttl_secs);
            terminate_child(&mut child).await;
            Ok(())
        }
    };

    let _ = tokio::fs::remove_file(&key_path).await;
    result
}

#[cfg(unix)]
async fn write_priv_key(pem: &str) -> Result<PathBuf> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let path = dir.join(format!("mirage-debug-shell-{pid}.key"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
    {
        use std::io::Write;
        let mut f = opts
            .open(&path)
            .with_context(|| format!("create key file {:?}", path))?;
        let mut buf = pem.to_owned();
        if !buf.ends_with('\n') {
            buf.push('\n');
        }
        f.write_all(buf.as_bytes())
            .with_context(|| format!("write key file {:?}", path))?;
    }
    Ok(path)
}

#[cfg(windows)]
async fn write_priv_key(pem: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let path = dir.join(format!("mirage-debug-shell-{pid}.key"));
    let mut buf = pem.to_owned();
    if !buf.ends_with('\n') {
        buf.push('\n');
    }
    tokio::fs::write(&path, buf.as_bytes())
        .await
        .with_context(|| format!("write key file {:?}", path))?;
    // Windows ACL hardening is best-effort; OpenSSH on Windows usually warns
    // but still accepts the key. Production deployment runs on *nix clients.
    Ok(path)
}

async fn terminate_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SIGTERM
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(s)) => tracing::info!("[debug-shell] ssh exited {:?}", s),
        Ok(Err(e)) => tracing::warn!("[debug-shell] ssh wait error: {}", e),
        Err(_) => {
            tracing::warn!("[debug-shell] ssh did not exit within 5s — SIGKILL");
            let _ = child.kill().await;
        }
    }
}
