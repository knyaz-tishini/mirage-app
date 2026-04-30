use std::{collections::VecDeque, net::SocketAddr, path::PathBuf, sync::Arc};

use clap::Parser;
use mirage_client_cli::{CliRunOptions, LogCaptureLayer};

/// mirage-client — local SOCKS5 proxy that routes traffic through the Mirage chain.
#[derive(Parser)]
#[command(name = "mirage-client", version, about)]
struct Args {
    /// Path to the ClientConfig JSON file.
    #[arg(long, short = 'c')]
    config: PathBuf,

    /// Local SOCKS5 listen address (default chain).
    #[arg(long, short = 'b', default_value = "127.0.0.1:19480")]
    bind: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tracing_subscriber::prelude::*;

    let log_buffer: Arc<std::sync::Mutex<VecDeque<String>>> =
        Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(2000)));

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let fmt_layer = tracing_subscriber::fmt::layer();
    let capture_layer = LogCaptureLayer::new(Arc::clone(&log_buffer), 2000);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(capture_layer)
        .init();

    let args = Args::parse();

    let opts = CliRunOptions::standalone(args.config, args.bind);
    mirage_client_cli::run(opts).await
}
