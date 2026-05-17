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
use dashmap::DashMap;
use mirage_chain_ipc::{
    MonitorTransportSpec, SupervisorMsg, WorkerMsg, WorkerState,
    pid_file_path, pipe_path,
    pipe::{PipePair, PipeListener},
};
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use mirage_client_core::{
    ChainHandle, ChainId, ChainMetrics, ChainSet, ChainState, HealthProbeConfig, PrewarmStrategy,
    monitor_link::{MonitorLink, MonitorTransport},
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
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// Default max idle seconds before orphaned worker exits.
const DEFAULT_ORPHAN_TIMEOUT_SECS: u64 = 3600;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("MIRAGE_LOG")
                .add_directive(
                    "mirage_chain_worker=debug".parse().expect("valid directive"),
                )
                // MonitorLink + OverChainChannel state transitions only — same
                // as supervisor. Worker hosts MonitorLink when monitor_link_host=true.
                .add_directive(
                    "mirage_client_core::monitor_link=info".parse().expect("valid directive"),
                )
                .add_directive(
                    "mirage_client_core::monitor_channel=info".parse().expect("valid directive"),
                )
                // Pool slot lifecycle (init / reconnect / lease) — visible to
                // diagnose pool-exhausted errors during chain dial issues.
                .add_directive(
                    "mirage_client_core::pool=info".parse().expect("valid directive"),
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

    // Capture mesh_id and client_id as strings for MonitorLink FRAME_AUTH.
    let mesh_id_str = client_config.mesh_id.clone();
    let client_id_str = client_config.client_id.clone();

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

    let pool = TunnelPool::new(chain.clone(), chain_protocols, pool_cfg, mesh_id, Arc::clone(&peer_table))
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

    // Channel for background tasks (MonitorLink watcher) to send WorkerMsg
    // back to the supervisor via the IPC pipe. The receive end lives inside
    // `supervisor_loop` / `handle_session`.
    let (outgoing_tx_raw, outgoing_rx) = mpsc::channel::<WorkerMsg>(16);
    let outgoing_tx = Arc::new(outgoing_tx_raw);

    let result = supervisor_loop(
        &mut listener,
        &chain_id,
        &chain,
        &mesh_id_str,
        &client_id_str,
        pid,
        started_at_unix,
        ipc,
        Arc::clone(&pool),
        mux_pool_for_loop,
        Arc::clone(&current_state),
        Arc::clone(&peer_table),
        stop_tx,
        outgoing_tx,
        outgoing_rx,
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
    chain_cfg: &mirage_core::config::ChainConfig,
    mesh_id: &str,
    node_client_id: &str,
    pid: u32,
    started_at_unix: u64,
    initial_ipc: PipePair,
    pool: Arc<TunnelPool>,
    mux_pool_opt: Option<Arc<MuxPool>>,
    state: Arc<tokio::sync::Mutex<WorkerState>>,
    peer_table: Arc<ArcSwap<PeerTable>>,
    stop_tx: watch::Sender<bool>,
    outgoing_tx: Arc<mpsc::Sender<WorkerMsg>>,
    mut outgoing_rx: mpsc::Receiver<WorkerMsg>,
    orphan_timeout_secs: u64,
) -> anyhow::Result<()> {
    let mut ipc = initial_ipc;

    loop {
        // Handle messages from the current supervisor connection.
        let disconnect_reason = handle_session(
            &mut ipc,
            chain_id,
            chain_cfg,
            mesh_id,
            node_client_id,
            Arc::clone(&pool),
            mux_pool_opt.as_ref().map(Arc::clone),
            Arc::clone(&state),
            Arc::clone(&peer_table),
            Arc::clone(&outgoing_tx),
            &mut outgoing_rx,
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
    chain_cfg: &mirage_core::config::ChainConfig,
    mesh_id: &str,
    node_client_id: &str,
    pool: Arc<TunnelPool>,
    mux_pool_opt: Option<Arc<MuxPool>>,
    state: Arc<tokio::sync::Mutex<WorkerState>>,
    peer_table: Arc<ArcSwap<PeerTable>>,
    outgoing_tx: Arc<mpsc::Sender<WorkerMsg>>,
    outgoing_rx: &mut mpsc::Receiver<WorkerMsg>,
    stop_tx: &watch::Sender<bool>,
) -> SessionEnd {
    loop {
        tokio::select! {
            // Background tasks (MonitorLink watcher) want to send a message
            // to the supervisor. Write it to the IPC pipe immediately.
            msg = outgoing_rx.recv() => {
                let Some(msg) = msg else {
                    // Channel closed — no more background tasks; keep running.
                    // Use a never-resolving future replacement in next iteration.
                    continue;
                };
                if let Err(e) = ipc.write_msg(&msg).await {
                    warn!(chain_id, "IPC write error sending outgoing message: {e} — treating as disconnect");
                    return SessionEnd::Disconnect;
                }
            }

            // Messages from supervisor.
            result = ipc.read_msg::<SupervisorMsg>() => {
        match result {
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
            Ok(Some(SupervisorMsg::SetupMonitorLink {
                monitor_urls,
                monitor_api_key,
                transports_json,
            })) => {
                info!(chain_id, "worker: SetupMonitorLink received — spawning MonitorLink");
                spawn_monitor_link_in_worker(
                    chain_id,
                    monitor_urls,
                    monitor_api_key,
                    transports_json,
                    mesh_id.to_string(),
                    node_client_id.to_string(),
                    Arc::clone(&peer_table),
                    Arc::clone(&pool),
                    mux_pool_opt.as_ref().map(Arc::clone),
                    chain_cfg.clone(),
                    Arc::clone(&outgoing_tx),
                );
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
        } // end match result
        } // end result = ipc.read_msg arm
        } // end tokio::select!
    } // end loop
} // end handle_session

/// Build a single-entry `ChainSet` for this worker's chain, construct
/// `MonitorLink`, spawn it, then watch for peer_table snapshots and forward
/// them to the supervisor via `outgoing_tx` as `WorkerMsg::PeerTableSnapshot`.
///
/// The worker's `TunnelPool` / `MuxPool` and the `MonitorLink` share the SAME
/// `Arc<ArcSwap<PeerTable>>` so the chain self-resolves from MonitorLink's
/// own snapshots (no chicken-and-egg: OverChain only activates when direct WS
/// fails; by then the first DirectWs snapshot has already populated the table).
fn spawn_monitor_link_in_worker(
    chain_id: &str,
    monitor_urls: Vec<String>,
    monitor_api_key: Option<String>,
    transports_json: String,
    mesh_id: String,
    node_client_id: String,
    peer_table: Arc<ArcSwap<PeerTable>>,
    pool: Arc<TunnelPool>,
    mux_pool_opt: Option<Arc<MuxPool>>,
    chain_cfg: mirage_core::config::ChainConfig,
    outgoing_tx: Arc<mpsc::Sender<WorkerMsg>>,
) {
    // Parse transport specs from JSON.
    let transport_specs: Vec<MonitorTransportSpec> = match serde_json::from_str(&transports_json) {
        Ok(v) => v,
        Err(e) => {
            warn!(chain_id, "SetupMonitorLink: failed to parse transports_json: {e} — MonitorLink not spawned");
            return;
        }
    };

    // Build a ChainHandle that wraps the worker's existing pools.
    // ChainConfig on the handle is needed for OverChain routing — we build a
    // minimal placeholder since MonitorLink only uses the ChainSet for dial,
    // and the actual dial config lives in TunnelPool / MuxPool already.
    let handle_id = ChainId(chain_id.to_string());
    // Clone mux_pool_opt before moving it into ChainHandle so the watcher task
    // can also hold a reference for peer_table notifications.
    let mux_for_handle = mux_pool_opt.as_ref().map(Arc::clone);
    let handle = Arc::new(ChainHandle {
        id: handle_id.clone(),
        config: Arc::new(ArcSwap::from_pointee(chain_cfg)),
        pool: Arc::clone(&pool),
        mux: mux_for_handle,
        metrics: ChainMetrics::new(),
        state: Arc::new(ArcSwap::from_pointee(ChainState::Active)),
    });

    // Build ChainSet with this single handle.
    let chain_map: Arc<DashMap<ChainId, Arc<ChainHandle>>> = Arc::new(DashMap::new());
    chain_map.insert(handle_id, handle);
    let chain_set = Arc::new(ChainSet {
        chains: chain_map,
        prewarm: PrewarmStrategy::default(),
        peer_table: Arc::clone(&peer_table),
        peer_table_notify: Arc::new(tokio::sync::Notify::new()),
        mesh_id: Vec::new(),  // MonitorLink doesn't use mesh_id directly
        probe_config: HealthProbeConfig::default(),
    });

    // Convert MonitorTransportSpec → MonitorTransport.
    let transports: Vec<MonitorTransport> = transport_specs
        .into_iter()
        .map(|spec| match spec {
            MonitorTransportSpec::DirectWs { url, sni } => {
                MonitorTransport::DirectWs { url, sni }
            }
            MonitorTransportSpec::OverChain { worker_socks5, monitor_addr, chain_id: cid } => {
                MonitorTransport::OverChain {
                    worker_socks5,
                    monitor_addr,
                    chain_id: ChainId(cid),
                }
            }
        })
        .collect();

    let (monitor_link, mut config_rx) = MonitorLink::new(
        monitor_urls,
        monitor_api_key,
        Arc::clone(&peer_table),
        chain_set,
        transports,
        mesh_id,
        node_client_id,
    );
    monitor_link.spawn();

    let chain_id_owned = chain_id.to_string();
    // Watcher task: on each new MonitorLink config with a new peer_table
    // generation, send WorkerMsg::PeerTableSnapshot to supervisor via outgoing_tx.
    tokio::spawn(async move {
        let mut last_generation: u64 = 0;
        loop {
            if config_rx.changed().await.is_err() {
                // MonitorLink dropped — watcher exits.
                break;
            }
            // Read the current peer_table directly from the ArcSwap (already
            // updated by MonitorLink::apply_config before the watch fires).
            let snapshot = peer_table.load();
            let generation = snapshot.generation;
            if generation <= last_generation && last_generation != 0 {
                continue;
            }
            last_generation = generation;
            match serde_json::to_string(&**snapshot) {
                Ok(json) => {
                    debug!(
                        chain_id = %chain_id_owned,
                        generation,
                        "worker: sending PeerTableSnapshot to supervisor"
                    );
                    let msg = WorkerMsg::PeerTableSnapshot {
                        generation,
                        peer_table_json: json,
                    };
                    if outgoing_tx.send(msg).await.is_err() {
                        break; // supervisor disconnected — stop
                    }
                    // Notify pools so they can re-dial with fresh keys.
                    pool.notify_peer_table_change();
                    if let Some(ref mp) = mux_pool_opt {
                        mp.notify_peer_table_change();
                    }
                }
                Err(e) => {
                    warn!(chain_id = %chain_id_owned, "worker: peer_table serialize error: {e}");
                }
            }
        }
    });
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
