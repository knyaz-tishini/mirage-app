//! mirage-l0-sentinel — binary entry point.
//!
//! Parses CLI args, reads sentinel TOML, delegates to [`mirage_l0_sentinel::run`].

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use mirage_l0_sentinel::SentinelConfig;

#[derive(Parser)]
#[command(name = "mirage-l0-sentinel", version, about = "Bootstrap mirage-client from monitor")]
struct Args {
    /// Path to sentinel TOML config file.
    #[arg(long, default_value = "sentinel.toml")]
    config: PathBuf,
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

    match rustls::crypto::CryptoProvider::install_default(
        rustls::crypto::ring::default_provider()
    ) {
        Ok(()) => tracing::info!("[sentinel] rustls crypto provider: ring (installed)"),
        Err(_) => tracing::info!("[sentinel] rustls crypto provider: already installed"),
    }

    let args = Args::parse();

    tracing::info!("[sentinel] loading config {:?}", args.config);
    let cfg_raw = std::fs::read_to_string(&args.config)
        .with_context(|| format!("cannot read sentinel config {:?}", args.config))?;
    let cfg: SentinelConfig = toml::from_str(&cfg_raw).context("failed to parse sentinel TOML")?;

    mirage_l0_sentinel::run(cfg).await
}
