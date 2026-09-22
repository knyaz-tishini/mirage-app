//! `mirage-subscriber-client` — the runnable binary.
//!
//! The commercial customer's own process: reads his delivery artefact,
//! parses it, refuses it BY NAME if it is malformed
//! (`mirage_subscriber_client::artefact`'s three-step discipline — parse,
//! then validate, then assemble), and otherwise stands up the local,
//! loopback-only SOCKS5 listener the platform's TUN binaries dial into
//! (`mirage2tun/crates/mirage-tun-{linux,windows,macos}/src/bin/mirage2tun.rs`).
//!
//! This binary is NOT an admin surface: its only two inputs are the
//! artefact path and the local bind address, and the bind address is
//! refused outright if it is not loopback — the customer has no flag that
//! picks a hop, a node, or a chain composition; that is all in the
//! artefact, sealed by the issuing panel.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use mirage_subscriber_client::{EntranceArtefact, SubscriberClient, SubscriberClientOptions};
use tracing::{error, info};

#[derive(Parser)]
#[command(
    name = "mirage-subscriber-client",
    version,
    about = "Mirage commercial subscriber client — local SOCKS5 proxy over the delivered entrance artefact"
)]
struct Args {
    /// Path to the delivery artefact (JSON) issued by the operator.
    #[arg(long)]
    artefact: PathBuf,

    /// Local SOCKS5 bind address. Must be a loopback address — this
    /// listener must never be reachable off this machine.
    #[arg(long, default_value = "127.0.0.1:11080")]
    bind: SocketAddr,
}

/// Standard L0 panic catcher (matches every other binary in this
/// workspace, e.g. `crates/debug-shell/src/main.rs`): log the location and
/// message through `tracing`, then a clean `exit(1)` — never the default
/// Rust abort, and never a raw backtrace in the customer's face.
fn install_panic_hook() {
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
        error!(location = %location, message = %msg, "panic — process exiting");
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::process::exit(1);
    }));
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    install_panic_hook();

    let args = Args::parse();

    if !args.bind.ip().is_loopback() {
        error!(
            bind = %args.bind,
            "refusing: --bind must be a loopback address — this listener must never be reachable off this machine"
        );
        return ExitCode::FAILURE;
    }

    let raw = match std::fs::read_to_string(&args.artefact) {
        Ok(raw) => raw,
        Err(e) => {
            error!(path = %args.artefact.display(), error = %e, "refusing: cannot read artefact file");
            return ExitCode::FAILURE;
        }
    };

    // Step 1: parse — never validates (`EntranceArtefact::from_json`).
    let artefact = match EntranceArtefact::from_json(&raw) {
        Ok(artefact) => artefact,
        Err(e) => {
            error!(error = %e, "refusing: artefact did not parse");
            return ExitCode::FAILURE;
        }
    };

    // Step 2: validate — refuses BY NAME, touches nothing else.
    if let Err(e) = artefact.validate() {
        error!(error = %e, "refusing: artefact failed validation");
        return ExitCode::FAILURE;
    }

    // Startup log: only what a customer may see — entrance count and mode,
    // never the private key, never a full artefact dump.
    info!(
        entrances = artefact.entrances.len(),
        mode = ?artefact.mode,
        bind = %args.bind,
        "mirage-subscriber-client: artefact validated, assembling local proxy"
    );

    let options = SubscriberClientOptions { socks5_bind: args.bind, ..Default::default() };

    // Step 3: assemble — only after a successful validate. `build()` itself
    // re-validates (cheap, pure, no side effects) and, when the artefact
    // carries `ArtefactMode::Country`, logs the named no-op warning per the
    // crate's own module doc (`mirage_subscriber_client::assembly`).
    let client = match SubscriberClient::build(&artefact, options).await {
        Ok(client) => client,
        Err(e) => {
            error!(error = %e, "refusing: failed to assemble subscriber client");
            return ExitCode::FAILURE;
        }
    };

    info!(bind = %client.socks5_bind, "mirage-subscriber-client: SOCKS5 listener ready — Ctrl-C to stop");

    if tokio::signal::ctrl_c().await.is_err() {
        error!("failed to install Ctrl-C handler — shutting down anyway");
    }

    info!("mirage-subscriber-client: shutdown requested");
    client.shutdown();

    ExitCode::SUCCESS
}
