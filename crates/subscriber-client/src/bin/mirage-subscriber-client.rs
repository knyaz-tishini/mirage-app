//! `mirage-subscriber-client` — the runnable binary.
//!
//! The commercial customer's own process: reads his delivery artefact,
//! parses it, refuses it BY NAME if it is malformed
//! (`mirage_subscriber_client::artefact`'s three-step discipline — parse,
//! then validate, then assemble), and otherwise stands up the local,
//! loopback-only SOCKS5 listener carrying every flow over one persistent
//! mux session to the customer's edge
//! (`mirage_subscriber_client::session::EdgeSession`). The
//! `countries` subcommand instead connects, prints the edge's country list,
//! and exits — it never starts the SOCKS5 listener.
//!
//! This binary is NOT an admin surface: its only inputs are the artefact
//! path and the local bind address, and the bind address is refused
//! outright if it is not loopback — the customer has no flag that picks a
//! hop, a node, or a chain composition; that is all the edge's own job.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mirage_subscriber_client::{
    exit_preference_from_mode, EdgeSession, EntranceArtefact, SubscriberClient,
    SubscriberClientOptions,
};
use tracing::{error, info};

#[derive(Parser)]
#[command(
    name = "mirage-subscriber-client",
    version,
    about = "Mirage commercial subscriber client — local SOCKS5 proxy over one persistent edge session"
)]
struct Args {
    /// Path to the delivery artefact (JSON) issued by the operator.
    #[arg(long)]
    artefact: PathBuf,

    /// Local SOCKS5 bind address. Must be a loopback address — this
    /// listener must never be reachable off this machine. Ignored by the
    /// `countries` subcommand.
    #[arg(long, default_value = "127.0.0.1:1080")]
    bind: SocketAddr,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the edge, print the countries it can serve (one per
    /// line), and exit. Never starts the local SOCKS5 listener.
    Countries,
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

fn read_and_validate_artefact(path: &PathBuf) -> Result<EntranceArtefact, ExitCode> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        error!(path = %path.display(), error = %e, "refusing: cannot read artefact file");
        ExitCode::FAILURE
    })?;

    // Step 1: parse — never validates (`EntranceArtefact::from_json`).
    let artefact = EntranceArtefact::from_json(&raw).map_err(|e| {
        error!(error = %e, "refusing: artefact did not parse");
        ExitCode::FAILURE
    })?;

    // Step 2: validate — refuses BY NAME, touches nothing else.
    artefact.validate().map_err(|e| {
        error!(error = %e, "refusing: artefact failed validation");
        ExitCode::FAILURE
    })?;

    Ok(artefact)
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

    let artefact = match read_and_validate_artefact(&args.artefact) {
        Ok(artefact) => artefact,
        Err(code) => return code,
    };

    match args.command {
        Some(Command::Countries) => run_countries(&artefact).await,
        None => run_proxy(&artefact, args.bind).await,
    }
}

/// Connect to the edge over its TCP-shaped entrances, print every country it
/// can serve, and exit — no SOCKS5 listener, no supervisor, no reconnect
/// loop; a single dial attempt is enough for this one-shot query.
async fn run_countries(artefact: &EntranceArtefact) -> ExitCode {
    let exit = match exit_preference_from_mode(&artefact.mode) {
        Ok(exit) => exit,
        Err(e) => {
            error!(error = %e, "refusing: artefact mode does not map to a valid exit preference");
            return ExitCode::FAILURE;
        }
    };
    let client_static_priv = match artefact.client_static_priv() {
        Ok(key) => key,
        Err(e) => {
            error!(error = %e, "refusing: cannot decode the subscriber's private key");
            return ExitCode::FAILURE;
        }
    };
    let mesh_id = artefact.mesh_id.as_bytes().to_vec();
    let tcp_entrances: Vec<_> =
        artefact.entrances.iter().filter(|e| !e.camouflage.is_udp()).cloned().collect();
    if tcp_entrances.is_empty() {
        error!("refusing: every entrance in the artefact is UDP-shaped, nothing to dial");
        return ExitCode::FAILURE;
    }

    match EdgeSession::connect_first_available(&tcp_entrances, &mesh_id, client_static_priv, exit, 0)
        .await
    {
        Ok((idx, session)) => {
            info!(entrance = %tcp_entrances[idx].addr, "mirage-subscriber-client: connected to edge");
            for cc in session.countries() {
                println!("{cc}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "refusing: could not reach the edge");
            ExitCode::FAILURE
        }
    }
}

/// Stand up the local SOCKS5 proxy over one persistent edge session — the
/// default mode.
async fn run_proxy(artefact: &EntranceArtefact, bind: SocketAddr) -> ExitCode {
    if !bind.ip().is_loopback() {
        error!(
            bind = %bind,
            "refusing: --bind must be a loopback address — this listener must never be reachable off this machine"
        );
        return ExitCode::FAILURE;
    }

    // Startup log: only what a customer may see — entrance count and mode,
    // never the private key, never a full artefact dump.
    info!(
        entrances = artefact.entrances.len(),
        mode = ?artefact.mode,
        bind = %bind,
        "mirage-subscriber-client: artefact validated, assembling local proxy"
    );

    let options = SubscriberClientOptions { socks5_bind: bind, ..Default::default() };

    let client = match SubscriberClient::build(artefact, options).await {
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
