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
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mirage_control_plane::ExitPreference;
use mirage_subscriber_client::{
    exit_preference_from_mode, publish_fatal, EdgeReason, EdgeSession, EntranceArtefact,
    SubscriberClient, SubscriberClientError, SubscriberClientOptions,
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

    /// Keep this JSON file current with the connector's state (connecting /
    /// connected / reconnecting / refused / fatal, a closed reason code, the
    /// requested and granted exit, the edge's country list). Rewritten
    /// atomically on every transition and every 5 s; removed on a clean
    /// shutdown. Ignored by the `countries` subcommand.
    #[arg(long)]
    status_file: Option<PathBuf>,

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
        Err(code) => {
            if args.command.is_none() {
                report_fatal(
                    args.status_file.as_deref(),
                    &ExitPreference::Auto,
                    Some(EdgeReason::BadArtefact),
                );
            }
            return code;
        }
    };

    match args.command {
        Some(Command::Countries) => run_countries(&artefact).await,
        None => run_proxy(&artefact, args.bind, args.status_file).await,
    }
}

/// Best effort: leave a `fatal` status document for a connector that cannot
/// start, so its supervisor learns why without parsing logs. A failure to
/// write it is logged and otherwise ignored — the process is exiting anyway.
fn report_fatal(status_file: Option<&Path>, requested: &ExitPreference, reason: Option<EdgeReason>) {
    let Some(path) = status_file else { return };
    if let Err(e) = publish_fatal(path, requested, reason) {
        error!(path = %path.display(), error = %e, "cannot write the fatal status file");
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
async fn run_proxy(
    artefact: &EntranceArtefact,
    bind: SocketAddr,
    status_file: Option<PathBuf>,
) -> ExitCode {
    // `validate()` already accepted the mode, so the fallback is unreachable;
    // it only keeps the fatal document well-formed if that ever changes.
    let requested = exit_preference_from_mode(&artefact.mode).unwrap_or(ExitPreference::Auto);

    if !bind.ip().is_loopback() {
        error!(
            bind = %bind,
            "refusing: --bind must be a loopback address — this listener must never be reachable off this machine"
        );
        report_fatal(status_file.as_deref(), &requested, None);
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

    let options = SubscriberClientOptions {
        socks5_bind: bind,
        status_file: status_file.clone(),
        ..Default::default()
    };

    let client = match SubscriberClient::build(artefact, options).await {
        Ok(client) => client,
        Err(e) => {
            error!(error = %e, "refusing: failed to assemble subscriber client");
            // A status-file failure has no reason code and nowhere to be
            // reported; every other refusal is published as `fatal`.
            if !matches!(e, SubscriberClientError::StatusFile { .. }) {
                report_fatal(status_file.as_deref(), &requested, e.status_reason());
            }
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
