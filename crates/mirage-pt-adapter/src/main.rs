//! `mirage-pt-adapter` — arc3 Phase-0 Tor Pluggable Transport adapter.
//!
//! Dispatches on which side Tor launched this process as, purely from
//! PT-v1 env var presence (contract §2): `TOR_PT_CLIENT_TRANSPORTS` set =>
//! client mode, `TOR_PT_SERVER_TRANSPORTS` set => server mode. Neither set
//! => hard error — this binary is a subprocess Tor launches, never run
//! standalone.
//!
//! **stdout is reserved for the PT-v1 protocol line stream** (`VERSION` /
//! `CMETHOD` / `SMETHOD` / ...) — ALL logging goes to stderr. A stray log
//! line on stdout desyncs Tor's line-by-line parser; this is the #1 way
//! real PT binaries break during bring-up (contract §2).

mod client_mode;
mod config;
mod pt_ipc;
mod server_mode;

use pt_ipc::{PtMode, PtStdout};
use tracing::{error, info};

#[tokio::main]
async fn main() {
    init_tracing();

    if let Err(err) = run().await {
        error!(error = %err, "mirage-pt-adapter: fatal error");
        std::process::exit(1);
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
}

async fn run() -> anyhow::Result<()> {
    let env = pt_ipc::parse_env()?;
    info!(
        version = %env.version,
        state_location = %env.state_location.display(),
        exit_on_stdin_close = env.exit_on_stdin_close,
        "mirage-pt-adapter: parsed PT-v1 managed-transport env"
    );

    if env.exit_on_stdin_close {
        spawn_stdin_close_watcher();
    }

    let mut ipc = PtStdout::new(tokio::io::stdout());
    ipc.version("1").await?;

    match env.mode {
        PtMode::Client { transports, proxy } => {
            if let Some(p) = proxy {
                info!(proxy = %p, "client_mode: TOR_PT_PROXY set — spike ignores upstream proxy config");
            }
            if let Err(e) = pt_ipc::validate_transports(&transports) {
                let bad_name = match &e {
                    pt_ipc::PtIpcError::UnknownTransport(n) => n.as_str(),
                    _ => "unknown",
                };
                ipc.cmethod_error(bad_name, "no-such-transport").await?;
                ipc.cmethods_done().await?;
                anyhow::bail!("TOR_PT_CLIENT_TRANSPORTS validation failed: {e}");
            }
            let cfg = config::ClientModeConfig::from_env();
            client_mode::run_client_mode(cfg, &mut ipc).await
        }
        PtMode::Server {
            transports,
            or_port,
            ext_or_port,
            auth_cookie_file,
            bind_addrs,
            transport_options,
        } => {
            info!(
                ?ext_or_port,
                ?auth_cookie_file,
                bind_addrs = ?bind_addrs,
                ?transport_options,
                "server_mode: ext_or_port/auth_cookie_file/bind_addrs/transport_options parsed but unused in this spike (contract §4)"
            );
            if let Err(e) = pt_ipc::validate_transports(&transports) {
                let bad_name = match &e {
                    pt_ipc::PtIpcError::UnknownTransport(n) => n.as_str(),
                    _ => "unknown",
                };
                ipc.smethod_error(bad_name, "no-such-transport").await?;
                ipc.smethods_done().await?;
                anyhow::bail!("TOR_PT_SERVER_TRANSPORTS validation failed: {e}");
            }
            let cfg = config::ServerModeConfig::from_or_port(or_port);
            server_mode::run_server_mode(cfg, &mut ipc).await
        }
    }
}

/// `TOR_PT_EXIT_ON_STDIN_CLOSE`: Tor closes this process's stdin as its
/// "you may exit now" signal on some platforms (contract §2, mirrors
/// lyrebird's documented behavior). Spike-level handling: drain stdin to
/// EOF, then exit cleanly — no graceful connection drain, matching the
/// rest of this binary's "spike, not shipping product" scope.
fn spawn_stdin_close_watcher() {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;

        let mut stdin = tokio::io::stdin();
        let mut sink = [0u8; 256];
        loop {
            match stdin.read(&mut sink).await {
                Ok(0) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        info!("stdin closed (TOR_PT_EXIT_ON_STDIN_CLOSE) — exiting");
        std::process::exit(0);
    });
}
