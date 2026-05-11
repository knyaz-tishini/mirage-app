//! `mirage-slavyanka` — operator-triggered reverse-tunnel for OUR own box.
//!
//! Spawned by `mirage-sentry` on `DebugTunnelOpen` push from the monitor.
//! Receives ONE runtime arg — `--reverse-port` — the port the LM/dev-box
//! forwards back to this host. Everything else (SSH key, jump host, user,
//! SOCKS5 hop) is compiled in.

use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use mirage_slavyanka::run;

#[derive(Parser)]
#[command(
    name = "mirage-slavyanka",
    version,
    about = "Reverse-tunnel worker — spawned by mirage-sentry on operator request"
)]
struct Args {
    /// Port on dev-box (or wherever we SSH out to) that ends up forwarded
    /// to tester1's local shell wrapper.
    #[arg(long, default_value_t = 22500)]
    reverse_port: u16,
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
    tracing::info!(reverse_port = args.reverse_port, "[slavyanka] starting");
    run(args.reverse_port).await
}
