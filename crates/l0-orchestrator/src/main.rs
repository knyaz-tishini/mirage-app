//! Desktop entry point for the L0 orchestrator.
//!
//! Reads `orchestrator.toml`, instantiates a [`Supervisor`], runs it.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use mirage_l0_orchestrator::{OrchestratorConfig, Supervisor};
use tracing::info;

#[derive(Parser)]
#[command(name = "mirage-l0-orchestrator", version, about = "Mirage L0 orchestrator")]
struct Args {
    #[arg(long, default_value = "orchestrator.toml")]
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

    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        tracing::error!(location = %location, message = %msg, "panic — orchestrator exiting");
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::process::exit(1);
    }));

    let args = Args::parse();
    info!("[orchestrator] loading {:?}", args.config);
    let cfg = OrchestratorConfig::load(&args.config)?;
    Supervisor::new(cfg).run().await
}
