//! mirage-chain-worker — single-chain SOCKS5 worker process.
//!
//! Receives IPC commands from supervisor over a named pipe (Windows) or
//! Unix socket (Linux/macOS). JSON-line framing identical to the old
//! stdin/stdout protocol.
//!
//! The worker is the **server** side of the IPC channel. It binds/listens,
//! and the supervisor connects as a client. This lets the worker survive
//! supervisor restarts: after the supervisor exits and is relaunched, it
//! simply reconnects to the same pipe.

use std::{net::SocketAddr, sync::Arc, time::SystemTime};

use anyhow::Context;
use mirage_chain_ipc::{
    SupervisorMsg, WorkerMsg, WorkerState,
    pid_file_path, pipe_path,
    pipe::{PipePair, PipeListener},
};
use mirage_client_core::{
    endpoint_protocol::EndpointProtocol,
    mux_pool::MuxPool,
    pool::TunnelPool,
    socks5::Socks5Server,
};
use mirage_core::{
    config::{ClientConfig, TcpProtocolId},
    routing::RoutingTable,
};
use mirage_protocol::{
    aliases::{mirage_base, mirage_tls},
    negotiation::ProtocolChain,
    traits::TcpProtocol,
};
use tokio::sync::watch;
use tracing::{info, warn};

/// Default max idle seconds before orphaned worker exits.
const DEFAULT_ORPHAN_TIMEOUT_SECS: u64 = 3600;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("MIRAGE_LOG").add_directive(
                "mirage_chain_worker=debug".parse().expect("valid directive"),
            ),
        )
        .with_writer(std::io::stderr)
        .init();

    let chain_id =
        std::env::var("MIRAGE_CHAIN_ID").unwrap_or_else(|_| "unknown".to_string());

    let orphan_timeout_secs: u64 = std::env::var("MIRAGE_WORKER_ORPHAN_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ORPHAN_TIMEOUT_SECS);

    info!(chain_id = %chain_id, "worker starting");

    // Write PID file immediately so the supervisor can discover us during
    // its startup scan.
    let pid = std::process::id();
    let pid_file = pid_file_path(&chain_id);
    if let Some(dir) = pid_file.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!("could not create PID dir {:?}: {e}", dir);
        }
    }
    if let Err(e) = std::fs::write(&pid_file, pid.to_string()) {
        warn!("could not write PID file {:?}: {e}", pid_file);
    }

    // Bind named pipe / Unix socket — worker is the server side.
    let pipe_addr = pipe_path(&chain_id);
    let mut listener = PipeListener::bind(&pipe_addr)
        .with_context(|| format!("binding IPC pipe {pipe_addr}"))?;

    info!(chain_id = %chain_id, pipe = %pipe_addr, "IPC pipe bound — waiting for supervisor");

    let started_at_unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Wait for the first supervisor connection and the Start message.
    let (socks5_bind, config_path, initial_ipc) =
        wait_for_start(&mut listener, &chain_id, pid, started_at_unix, WorkerState::Starting).await?;

    let addr: SocketAddr = socks5_bind.parse().with_context(|| {
        format!("invalid socks5_bind address '{socks5_bind}'")
    })?;

    // Load and parse config.
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read config {config_path}"))?;
    let client_config: ClientConfig = serde_json::from_str(&raw)
        .with_context(|| format!("parse config {config_path}"))?;

    // Find the chain this worker is responsible for.
    let chain = client_config
        .chains
        .iter()
        .find(|c| c.chain_id == chain_id)
        .ok_or_else(|| anyhow::anyhow!("chain '{}' not found in config", chain_id))?
        .clone();

    // Build protocol chain.
    let mut protocols: Vec<Box<dyn TcpProtocol>> = Vec::new();
    protocols.push(Box::new(EndpointProtocol::new(
        mirage_base(),
        TcpProtocolId::Mirage,
    )));
    if client_config.psk.is_some() {
        protocols.push(Box::new(EndpointProtocol::new(
            mirage_tls("cloudflare.com", vec!["h2".into(), "http/1.1".into()]),
            TcpProtocolId::MirageShadowTls,
        )));
    }
    let chain_protocols = Arc::new(ProtocolChain::new(protocols));

    // Build tunnel pool.
    let mut pool_cfg = client_config.pool.clone();
    if client_config.use_mux {
        pool_cfg.pool_size = 3;
    }

    let mux_pool_opt: Option<Arc<MuxPool>> = if client_config.use_mux {
        match MuxPool::new(chain.clone(), Arc::clone(&chain_protocols), pool_cfg.clone()).await {
            Ok(mp) => {
                info!(chain_id = %chain_id, "mux pool ready");
                Some(mp)
            }
            Err(e) => {
                warn!(chain_id = %chain_id, "mux pool init failed, falling back to TCP-only: {e}");
                None
            }
        }
    } else {
        None
    };

    let pool = TunnelPool::new(chain, chain_protocols, pool_cfg)
        .await
        .map_err(|e| anyhow::anyhow!("TunnelPool::new failed: {e}"))?;

    let router = Arc::new(RoutingTable::new(&client_config.routing));
    let config_arc = Arc::new(client_config.clone());

    // Announce readiness to the first supervisor.
    let mut ipc = initial_ipc;
    ipc.write_msg(&WorkerMsg::Ready {
        chain_id: chain_id.clone(),
        pid,
    })
    .await?;
    info!(chain_id = %chain_id, pid, "sent Ready");

    // Channel to signal SOCKS5 server to stop.
    let (stop_tx, stop_rx) = watch::channel(false);

    // Spawn SOCKS5 server.
    let server = Socks5Server::new(addr);
    let server_handle = tokio::spawn({
        let pool = Arc::clone(&pool);
        let mut stop_rx = stop_rx.clone();
        async move {
            let serve_fut = if mux_pool_opt.is_some() {
                tokio::spawn({
                    let pool = Arc::clone(&pool);
                    async move { server.run(pool, mux_pool_opt, config_arc, router).await }
                })
            } else {
                tokio::spawn({
                    let pool = Arc::clone(&pool);
                    async move { server.run_simple(pool).await }
                })
            };
            tokio::select! {
                result = serve_fut => {
                    if let Ok(Err(e)) = result {
                        tracing::error!(error = %e, "SOCKS5 server exited with error");
                    }
                }
                _ = stop_rx.changed() => {
                    // Drain signal received — server_handle task exits.
                }
            }
        }
    });

    // IPC supervisor loop — handles current session + reconnects.
    let current_state = Arc::new(tokio::sync::Mutex::new(WorkerState::Ready));

    let result = supervisor_loop(
        &mut listener,
        &chain_id,
        pid,
        started_at_unix,
        ipc,
        Arc::clone(&pool),
        Arc::clone(&current_state),
        stop_tx,
        orphan_timeout_secs,
    )
    .await;

    // If loop returned with a drain request, wait for server to finish.
    let _ = server_handle.await;

    // Remove PID file on clean exit.
    if let Err(e) = std::fs::remove_file(&pid_file) {
        warn!("could not remove PID file {:?}: {e}", pid_file);
    }

    result
}

/// Run the IPC command loop across multiple supervisor connections.
///
/// Returns `Ok(())` when the worker should exit cleanly (Drain completed or
/// Restart acknowledged). Returns `Err` on fatal internal errors.
async fn supervisor_loop(
    listener: &mut PipeListener,
    chain_id: &str,
    pid: u32,
    started_at_unix: u64,
    initial_ipc: PipePair,
    pool: Arc<TunnelPool>,
    state: Arc<tokio::sync::Mutex<WorkerState>>,
    stop_tx: watch::Sender<bool>,
    orphan_timeout_secs: u64,
) -> anyhow::Result<()> {
    let mut ipc = initial_ipc;

    loop {
        // Handle messages from the current supervisor connection.
        let disconnect_reason = handle_session(
            &mut ipc,
            chain_id,
            Arc::clone(&pool),
            Arc::clone(&state),
            &stop_tx,
        )
        .await;

        match disconnect_reason {
            SessionEnd::Exit => {
                // Drain completed — worker should exit.
                info!(chain_id, "drain/exit complete — worker exiting");
                return Ok(());
            }
            SessionEnd::Disconnect => {
                // Supervisor disconnected; wait for reconnect or orphan timeout.
                info!(chain_id, "supervisor disconnected — waiting for reconnect");
            }
        }

        // Orphan wait: accept next supervisor connection or timeout.
        let orphan_timeout = tokio::time::Duration::from_secs(orphan_timeout_secs);
        match tokio::time::timeout(orphan_timeout, listener.accept()).await {
            Err(_) => {
                warn!(
                    chain_id,
                    timeout_secs = orphan_timeout_secs,
                    "no supervisor reconnected — orphan timeout, exiting"
                );
                return Ok(());
            }
            Ok(Err(e)) => {
                warn!(chain_id, "IPC accept error: {e} — exiting");
                return Ok(());
            }
            Ok(Ok(new_ipc)) => {
                ipc = new_ipc;
                info!(chain_id, "supervisor reconnected");

                // Read current state to include in Hello.
                let cur_state = *state.lock().await;

                // Send Hello to the newly connected supervisor.
                let version = env!("CARGO_PKG_VERSION").to_string();
                if let Err(e) = ipc
                    .write_msg(&WorkerMsg::Hello {
                        chain_id: chain_id.to_string(),
                        pid,
                        started_at_unix,
                        version,
                        state: cur_state,
                    })
                    .await
                {
                    warn!(chain_id, "failed to send Hello on reconnect: {e}");
                    // Loop again to wait for another reconnect attempt.
                    continue;
                }

                // Read supervisor's response: Continue or Restart.
                match ipc.read_msg::<SupervisorMsg>().await {
                    Ok(Some(SupervisorMsg::Continue)) => {
                        info!(chain_id, "supervisor sent Continue — resuming");
                    }
                    Ok(Some(SupervisorMsg::Restart { reason })) => {
                        info!(chain_id, reason = %reason, "supervisor sent Restart — draining");
                        let _ = stop_tx.send(true);
                        return Ok(());
                    }
                    Ok(Some(other)) => {
                        warn!(chain_id, ?other, "unexpected message after Hello — treating as Continue");
                    }
                    Ok(None) => {
                        warn!(chain_id, "supervisor closed pipe immediately after Hello");
                        continue;
                    }
                    Err(e) => {
                        warn!(chain_id, "error reading Continue/Restart: {e}");
                        continue;
                    }
                }
            }
        }
    }
}

/// Reason a supervisor session ended.
enum SessionEnd {
    /// Worker should exit (drain completed or Shutdown received).
    Exit,
    /// Supervisor disconnected; worker should wait for reconnect.
    Disconnect,
}

/// Run one IPC session until supervisor disconnects or sends Drain/Shutdown.
async fn handle_session(
    ipc: &mut PipePair,
    chain_id: &str,
    pool: Arc<TunnelPool>,
    state: Arc<tokio::sync::Mutex<WorkerState>>,
    stop_tx: &watch::Sender<bool>,
) -> SessionEnd {
    loop {
        match ipc.read_msg::<SupervisorMsg>().await {
            Ok(None) => {
                warn!(chain_id, "supervisor disconnected (EOF on pipe)");
                return SessionEnd::Disconnect;
            }
            Err(e) => {
                warn!(chain_id, "IPC read error: {e} — treating as disconnect");
                return SessionEnd::Disconnect;
            }
            Ok(Some(SupervisorMsg::Drain { timeout_secs })) => {
                info!(chain_id, timeout_secs, "received Drain");
                let drain_timeout_secs = timeout_secs;
                let _ = stop_tx.send(true);
                *state.lock().await = WorkerState::Draining;

                // Drain phase.
                let drain_deadline = tokio::time::Instant::now()
                    + tokio::time::Duration::from_secs(drain_timeout_secs);
                loop {
                    let stats = pool.pool_stats().await;
                    let active = stats.leased as u32;
                    if ipc
                        .write_msg(&WorkerMsg::Draining {
                            chain_id: chain_id.to_string(),
                            active_connections: active,
                        })
                        .await
                        .is_err()
                    {
                        return SessionEnd::Disconnect;
                    }
                    if active == 0 {
                        break;
                    }
                    if tokio::time::Instant::now() >= drain_deadline {
                        warn!(chain_id, active, "drain timeout reached — forcing exit");
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }

                let _ = ipc
                    .write_msg(&WorkerMsg::Drained {
                        chain_id: chain_id.to_string(),
                    })
                    .await;
                info!(chain_id, "drained — exiting");
                *state.lock().await = WorkerState::Exiting;
                return SessionEnd::Exit;
            }
            Ok(Some(SupervisorMsg::Shutdown)) => {
                info!(chain_id, "received Shutdown — exiting immediately");
                let _ = stop_tx.send(true);
                *state.lock().await = WorkerState::Exiting;
                return SessionEnd::Exit;
            }
            Ok(Some(SupervisorMsg::Ping)) => {
                let stats = pool.pool_stats().await;
                let cur_state = *state.lock().await;
                let _ = ipc
                    .write_msg(&WorkerMsg::Status {
                        chain_id: chain_id.to_string(),
                        active_connections: stats.leased as u32,
                        total_connections: 0,
                        state: cur_state,
                    })
                    .await;
            }
            Ok(Some(SupervisorMsg::Start { .. })) => {
                warn!(chain_id, "received duplicate Start — ignoring");
            }
            Ok(Some(SupervisorMsg::Continue)) => {
                // Received mid-session (shouldn't happen); ignore.
                warn!(chain_id, "received Continue mid-session — ignoring");
            }
            Ok(Some(SupervisorMsg::Restart { reason })) => {
                info!(chain_id, reason = %reason, "received Restart mid-session — draining");
                let _ = stop_tx.send(true);
                *state.lock().await = WorkerState::Exiting;
                return SessionEnd::Exit;
            }
        }
    }
}

/// Wait for the first supervisor connection and the `Start` message.
///
/// The worker sends `Hello` immediately on connect (with `Starting` state).
/// After `Hello`, the supervisor either sends `Start` (fresh spawn) or
/// `Continue` (reattach path — but this function is only called on first boot
/// so `Start` is expected).
async fn wait_for_start(
    listener: &mut PipeListener,
    chain_id: &str,
    pid: u32,
    started_at_unix: u64,
    worker_state: WorkerState,
) -> anyhow::Result<(String, String, PipePair)> {
    loop {
        let mut ipc = listener.accept().await
            .with_context(|| "accepting first supervisor connection")?;

        info!(chain_id, "supervisor connected (initial)");

        // Send Hello immediately.
        let version = env!("CARGO_PKG_VERSION").to_string();
        ipc.write_msg(&WorkerMsg::Hello {
            chain_id: chain_id.to_string(),
            pid,
            started_at_unix,
            version,
            state: worker_state,
        })
        .await
        .with_context(|| "sending Hello")?;

        // Read Start (or Shutdown to abort).
        loop {
            match ipc.read_msg::<SupervisorMsg>().await? {
                None => {
                    warn!(chain_id, "supervisor closed pipe before Start — retrying");
                    break; // outer loop: wait for next connection
                }
                Some(SupervisorMsg::Start {
                    chain_id: _,
                    socks5_bind,
                    config_path,
                }) => {
                    info!(chain_id, bind = %socks5_bind, "received Start");
                    return Ok((socks5_bind, config_path, ipc));
                }
                Some(SupervisorMsg::Shutdown) => {
                    info!(chain_id, "Shutdown before Start — exiting");
                    std::process::exit(0);
                }
                Some(other) => {
                    warn!(chain_id, ?other, "unexpected message before Start — ignoring");
                }
            }
        }
    }
}
