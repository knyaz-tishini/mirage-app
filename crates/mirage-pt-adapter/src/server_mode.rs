//! Server/bridge mode (contract §4/§5.3).
//!
//! **No Mirage-wire code in this file at all.** The Mirage exit hop is a
//! genuine, unmodified `mirage-node` process running separately on the
//! bridge box as a `{"mode":{"exit":"direct"}}` listener (deployed at LIVE
//! setup, contract §10 checklist step 9 — out of scope here). This file's
//! only two jobs:
//!
//! 1. PT-v1 `SMETHOD` reporting — reports the mirage-node exit's real
//!    public listener address, NOT this forwarder's own bind addr.
//! 2. The §5.3 loopback-SSRF micro-forwarder: a tiny always-on TCP proxy
//!    from `forwarder_bind_addr` (non-loopback, passes `mirage-node`'s
//!    exit-target SSRF guard) to the real, loopback-bound Tor `ORPort`.
//!    This is the ONLY new networking code in server mode — pure TCP
//!    plumbing, no PT byte-framing, no Mirage awareness.

use std::net::SocketAddr;

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::config::ServerModeConfig;
use crate::pt_ipc::{PtStdout, TRANSPORT_NAME};

pub async fn run_server_mode(
    cfg: ServerModeConfig,
    ipc: &mut PtStdout<tokio::io::Stdout>,
) -> anyhow::Result<()> {
    let listener = match TcpListener::bind(cfg.forwarder_bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            let _ = ipc
                .smethod_error(
                    TRANSPORT_NAME,
                    &format!("forwarder bind {}: {e}", cfg.forwarder_bind_addr),
                )
                .await;
            let _ = ipc.smethods_done().await;
            return Err(e.into());
        }
    };

    // SMETHOD reports the mirage-node exit's real public listener address
    // (contract §4/§6.3) — NOT this forwarder's own bind addr. Tor does
    // not act on this value beyond syntactic validity; the mismatch vs.
    // `TOR_PT_SERVER_BINDADDR` is intentional, see contract §6.3.
    ipc.smethod(TRANSPORT_NAME, cfg.exit_public_addr).await?;
    ipc.smethods_done().await?;
    info!(
        forwarder_bind_addr = %cfg.forwarder_bind_addr,
        or_port = %cfg.or_port,
        exit_public_addr = %cfg.exit_public_addr,
        "server_mode: forwarder listening"
    );

    loop {
        let (inbound, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "server_mode: accept error");
                continue;
            }
        };
        let or_port = cfg.or_port;
        tokio::spawn(async move {
            if let Err(e) = forward_one(inbound, or_port).await {
                warn!(error = %e, peer = %peer, "server_mode: forward error");
            }
        });
    }
}

/// Dial the real (loopback) ORPort and splice — the entire forwarder job.
async fn forward_one(mut inbound: TcpStream, or_port: SocketAddr) -> anyhow::Result<()> {
    let mut outbound = TcpStream::connect(or_port).await?;
    let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
    Ok(())
}
