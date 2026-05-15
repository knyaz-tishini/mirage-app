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
use arc_swap::ArcSwap;
use mirage_chain_ipc::{
    SupervisorMsg, WorkerMsg, WorkerState,
    pid_file_path, pipe_path,
    pipe::{PipePair, PipeListener},
};
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use mirage_client_core::{
    mux_pool::MuxPool,
    pool::TunnelPool,
    socks5::Socks5Server,
};
use mirage_core::{
    config::{ClientConfig, TcpProtocolId},
    peer_table::PeerTable,
    routing::RoutingTable,
};
use mirage_protocol::{
    endpoint_protocol::EndpointProtocol as ProtocolEP,
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

    std::panic::set_hook(Box::new(|info| {
        let location = info.location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        tracing::error!(location = %location, message = %msg, "panic — process exiting for respawn");
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::process::exit(1);
    }));

    let chain_id =
        std::env::var("MIRAGE_CHAIN_ID").unwrap_or_else(|_| "unknown".to_string());

    let orphan_timeout_secs: u64 = std::env::var("MIRAGE_WORKER_ORPHAN_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ORPHAN_TIMEOUT_SECS);

    info!(chain_id = %chain_id, "worker starting");

    // ── Memory guard ──────────────────────────────────────────────────────────
    // Exit cleanly: supervisor will detect the exit and respawn.
    let _memory_guard = MemoryGuard::spawn(
        MemoryGuardConfig::default(),
        Box::new(|| {
            tracing::warn!("worker memory guard: fatal threshold — exiting for respawn");
            std::process::exit(0);
        }),
    );

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
    let (socks5_bind, config_path, initial_peer_table_json, initial_ipc) =
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

    // Build protocol chain — single Mirage wire for the entry hop.
    let mesh_id = client_config.mesh_id.as_bytes().to_vec();
    use mirage_core::config::{AuthMode, MirageEndpointConfig, TransportCamouflage, TransportKind};
    let entry_camouflage = chain.hops.first()
        .and_then(|h| h.camouflage.clone())
        .unwrap_or(TransportCamouflage::None);
    let entry_byte_cipher = chain.hops.first().and_then(|h| h.byte_cipher.clone());
    let ep_config = MirageEndpointConfig {
        transport: TransportKind::Tcp,
        auth: AuthMode::Blake3,
        camouflage: entry_camouflage,
        byte_cipher: None,
        byte_cipher_mode: entry_byte_cipher,
    };
    let proto: Box<dyn TcpProtocol> = Box::new(ProtocolEP::new(
        ep_config, TcpProtocolId::Mirage, mesh_id.clone(),
    ));
    let chain_protocols = Arc::new(ProtocolChain::new(vec![proto]));

    // Build tunnel pool.
    let mut pool_cfg = client_config.pool.clone();
    if client_config.use_mux {
        pool_cfg.pool_size = 3;
    }

    // Build initial peer_table: use snapshot from supervisor if provided,
    // otherwise fall back to empty (only hardcoded public_key hops will dial).
    let initial_pt = initial_peer_table_json
        .as_deref()
        .and_then(|json| match serde_json::from_str::<PeerTable>(json) {
            Ok(pt) => {
                info!(
                    chain_id = %chain_id,
                    generation = pt.generation,
                    peers = pt.peers.len(),
                    "worker: loaded initial peer_table from Start"
                );
                Some(pt)
            }
            Err(e) => {
                warn!(chain_id = %chain_id, "worker: failed to parse initial peer_table_json: {e} — using empty");
                None
            }
        })
        .unwrap_or_else(PeerTable::empty);
    let peer_table: Arc<ArcSwap<PeerTable>> = Arc::new(ArcSwap::from_pointee(initial_pt));

    let mux_pool_opt: Option<Arc<MuxPool>> = if client_config.use_mux {
        match MuxPool::new(
            chain.clone(),
            Arc::clone(&chain_protocols),
            pool_cfg.clone(),
            mesh_id.clone(),
            Arc::clone(&peer_table),
        ).await {
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

    let pool = TunnelPool::new(chain, chain_protocols, pool_cfg, mesh_id, Arc::clone(&peer_table))
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
    // Clone for supervisor_loop; original moves into server closure below.
    let mux_pool_for_loop = mux_pool_opt.as_ref().map(Arc::clone);
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
        mux_pool_for_loop,
        Arc::clone(&current_state),
        Arc::clone(&peer_table),
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
    mux_pool_opt: Option<Arc<MuxPool>>,
    state: Arc<tokio::sync::Mutex<WorkerState>>,
    peer_table: Arc<ArcSwap<PeerTable>>,
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
            mux_pool_opt.as_ref().map(Arc::clone),
            Arc::clone(&state),
            Arc::clone(&peer_table),
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
    mux_pool_opt: Option<Arc<MuxPool>>,
    state: Arc<tokio::sync::Mutex<WorkerState>>,
    peer_table: Arc<ArcSwap<PeerTable>>,
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
            Ok(Some(SupervisorMsg::PeerTableUpdate { peer_table_json })) => {
                match serde_json::from_str::<PeerTable>(&peer_table_json) {
                    Ok(pt) => {
                        info!(
                            chain_id,
                            generation = pt.generation,
                            peers = pt.peers.len(),
                            "worker: PeerTableUpdate applied"
                        );
                        peer_table.store(Arc::new(pt));
                        pool.notify_peer_table_change();
                        if let Some(mp) = &mux_pool_opt {
                            mp.notify_peer_table_change();
                        }
                    }
                    Err(e) => {
                        warn!(chain_id, "worker: PeerTableUpdate parse error: {e}");
                    }
                }
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
/// Returns `(socks5_bind, config_path, peer_table_json, ipc)`.
///
/// The worker sends `Hello` immediately on connect (with `Starting` state).
/// After `Hello`, the supervisor either sends `Start` (fresh spawn) or
/// `Continue` (reattach path — but this function is only called on first boot
/// so `Start` is expected).  `PeerTableUpdate` arriving before `Start` is
/// handled and stored; the latest JSON is returned together with `Start`.
async fn wait_for_start(
    listener: &mut PipeListener,
    chain_id: &str,
    pid: u32,
    started_at_unix: u64,
    worker_state: WorkerState,
) -> anyhow::Result<(String, String, Option<String>, PipePair)> {
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

        // Read Start (or Shutdown to abort).  PeerTableUpdate may arrive
        // before Start if supervisor already has a snapshot — buffer the
        // latest JSON so the caller can initialise the ArcSwap immediately.
        let mut buffered_peer_table_json: Option<String> = None;
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
                    peer_table_json,
                }) => {
                    info!(chain_id, bind = %socks5_bind, "received Start");
                    // Prefer peer_table_json from Start; fall back to any
                    // PeerTableUpdate that arrived before Start.
                    let resolved_pt = peer_table_json.or(buffered_peer_table_json);
                    return Ok((socks5_bind, config_path, resolved_pt, ipc));
                }
                Some(SupervisorMsg::PeerTableUpdate { peer_table_json }) => {
                    info!(chain_id, "PeerTableUpdate received before Start — buffering");
                    buffered_peer_table_json = Some(peer_table_json);
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
