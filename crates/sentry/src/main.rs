//! `mirage-sentry` — entry point. No CLI args, no config file — everything
//! is compiled in. Same panic-hook / memory-guard / tracing scaffolding
//! used by every other L0 binary.

use std::time::Duration;

use anyhow::Result;
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use mirage_sentry::run;

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

    tracing::info!("[sentry] starting (compiled-in config, no .toml)");
    run().await
}
