//! mirage-desktop-cli — single launcher for the desktop L0 stack.
//!
//! Spawns `mirage-client-cli.exe` and `mirage2tun.exe` as child processes,
//! waits for cli to be ready, then launches tun. Ctrl-C cascades kill.
//!
//! Layout expected next to this exe:
//!   mirage-desktop-cli.exe
//!   mirage-client-cli.exe
//!   mirage2tun.exe
//!   wintun.dll
//!   client-config.json   (cli reads this)
//!   tun.json             (tun reads this)

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::process::{Child, Command};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "mirage-desktop-cli", version, about = "Launch cli + tun together")]
struct Args {
    /// Path to client-config.json (cli reads it). Defaults to `client-config.json` next to the exe.
    #[arg(long)]
    client_config: Option<PathBuf>,

    /// Wait time before starting tun (seconds). Gives cli time to bind SOCKS5.
    #[arg(long, default_value = "2")]
    cli_warmup_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    let exe_dir = std::env::current_exe()
        .context("get current exe path")?
        .parent()
        .context("exe has no parent dir")?
        .to_path_buf();

    let cli_exe = exe_dir.join("mirage-client-cli.exe");
    let tun_exe = exe_dir.join("mirage2tun.exe");
    let client_config = args
        .client_config
        .unwrap_or_else(|| exe_dir.join("client-config.json"));

    for (label, p) in [("cli", &cli_exe), ("tun", &tun_exe), ("client-config", &client_config)] {
        if !p.exists() {
            anyhow::bail!("missing {}: {}", label, p.display());
        }
    }

    info!("[desktop-cli] exe_dir = {}", exe_dir.display());
    info!("[desktop-cli] starting cli: {}", cli_exe.display());

    let mut cli_child = Command::new(&cli_exe)
        .arg("--config").arg(&client_config)
        .current_dir(&exe_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn cli {}", cli_exe.display()))?;

    info!("[desktop-cli] cli pid = {:?}, warming up {}s", cli_child.id(), args.cli_warmup_secs);
    tokio::time::sleep(Duration::from_secs(args.cli_warmup_secs)).await;

    // Verify cli still alive after warmup.
    if let Ok(Some(status)) = cli_child.try_wait() {
        anyhow::bail!("cli exited during warmup with {:?}", status);
    }

    info!("[desktop-cli] starting tun: {}", tun_exe.display());
    let mut tun_child = Command::new(&tun_exe)
        .arg("debug-run")
        .current_dir(&exe_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn tun {}", tun_exe.display()))?;

    info!("[desktop-cli] tun pid = {:?}", tun_child.id());
    info!("[desktop-cli] both children running — Ctrl-C to stop");

    let exit_code = supervise(&mut cli_child, &mut tun_child).await;
    info!("[desktop-cli] exit {}", exit_code);
    std::process::exit(exit_code);
}

async fn supervise(cli: &mut Child, tun: &mut Child) -> i32 {
    tokio::select! {
        r = cli.wait() => {
            warn!("[desktop-cli] cli exited first: {:?} — terminating tun", r);
            let _ = tun.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(5), tun.wait()).await;
            r.ok().and_then(|s| s.code()).unwrap_or(1)
        }
        r = tun.wait() => {
            warn!("[desktop-cli] tun exited first: {:?} — terminating cli", r);
            let _ = cli.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(5), cli.wait()).await;
            r.ok().and_then(|s| s.code()).unwrap_or(1)
        }
        _ = tokio::signal::ctrl_c() => {
            info!("[desktop-cli] Ctrl-C — stopping both children");
            let _ = tun.kill().await;
            let _ = cli.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(5), tun.wait()).await;
            let _ = tokio::time::timeout(Duration::from_secs(5), cli.wait()).await;
            0
        }
    }
}

// silence unused import warning when compiled without features
#[allow(dead_code)]
fn _unused(_: &dyn std::error::Error) { error!("unused"); }
