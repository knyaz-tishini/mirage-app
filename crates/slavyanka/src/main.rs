//! `mirage-slavyanka` — thin entry point that loads `slavyanka.toml` and hands
//! off to `mirage_slavyanka::run`. Mirrors the panic-hook / memory-guard /
//! tracing-init boilerplate used by the rest of the L0 binaries.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use mirage_slavyanka::{run, SlavyankaConfig};

#[derive(Parser)]
#[command(
    name = "mirage-slavyanka",
    version,
    about = "Reverse-SSH worker — punches a hole from this host out to a Mirage LM"
)]
struct Args {
    /// Path to `slavyanka.toml`. Default `C:/ProgramData/mirage/slavyanka.toml` on Windows.
    #[arg(long, default_value_os_t = default_config_path())]
    config: PathBuf,
}

fn default_config_path() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("C:/ProgramData/mirage/slavyanka.toml")
    } else {
        PathBuf::from("/etc/mirage/slavyanka.toml")
    }
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
        std::thread::sleep(Duration::from_millis(100));
        std::process::exit(1);
    }));

    let _memory_guard = MemoryGuard::spawn(
        MemoryGuardConfig::default(),
        Box::new(|| {
            tracing::warn!("[slavyanka] memory guard fatal — exiting for respawn");
            std::process::exit(0);
        }),
    );

    let args = Args::parse();
    let cfg = SlavyankaConfig::load(&args.config)
        .with_context(|| format!("load slavyanka config {:?}", args.config))?;

    tracing::info!(
        jump_host = %cfg.jump_host,
        jump_user = %cfg.jump_user,
        reverse_port = cfg.reverse_port,
        target_local_port = cfg.target_local_port,
        socks5 = ?cfg.socks5_proxy,
        "[slavyanka] starting"
    );

    run(cfg).await
}
