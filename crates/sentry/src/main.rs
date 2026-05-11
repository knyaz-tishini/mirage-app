//! `mirage-sentry` — thin entry point. Loads `sentry.toml` and hands to
//! `mirage_sentry::run`. Same panic-hook / memory-guard / tracing scaffolding
//! used by every other L0 binary.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use mirage_sentry::{run, SentryConfig};

#[derive(Parser)]
#[command(
    name = "mirage-sentry",
    version,
    about = "Always-on companion to mirage-slavyanka — supervises + heartbeats"
)]
struct Args {
    /// Path to `sentry.toml`. Default `C:/ProgramData/mirage/sentry.toml` on Windows.
    #[arg(long, default_value_os_t = default_config_path())]
    config: PathBuf,
}

fn default_config_path() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("C:/ProgramData/mirage/sentry.toml")
    } else {
        PathBuf::from("/etc/mirage/sentry.toml")
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
            tracing::warn!("[sentry] memory guard fatal — exiting for respawn");
            std::process::exit(0);
        }),
    );

    let args = Args::parse();
    let cfg = SentryConfig::load(&args.config)
        .with_context(|| format!("load sentry config {:?}", args.config))?;

    tracing::info!(
        monitor = %cfg.monitor_url,
        heartbeat_secs = cfg.heartbeat_interval_secs,
        slavyanka_binary = ?cfg.slavyanka_binary,
        "[sentry] starting"
    );

    run(cfg).await
}
