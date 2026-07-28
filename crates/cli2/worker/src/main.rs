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

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::SystemTime};

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
    monitor_link::{MonitorLink, MonitorTransport, WsServerFrame},
    mux_pool::MuxPool,
    pool::TunnelPool,
    socks5::{Socks5Server, SocksFailurePolicy},
    wire_health::WireHealthMap,
};
use mirage_core::{
    config::{ClientConfig, ProtocolId, ReplyMode, TcpProtocolId, TunnelPoolConfig, WireNature},
    mirage::mesh::PeerTable,
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
    // Wire transport fields from ClientConfig (task #78).
    let wire_addr_str = client_config.monitor_wire_addr.clone();
    let wire_pubkey_str = client_config.monitor_wire_pubkey_hex.clone();
    // Blind-token credential fields from ClientConfig.
    let operator_pub_hex_str = client_config.operator_pub_hex.clone();
    let enrol_token_str = client_config.enrol_token.clone();
    // Ring-membership (0x04) fields from ClientConfig.
    let ring_member_sk_hex_str = client_config.ring_member_sk_hex.clone();
    let ring_path_str = client_config.ring_path.clone();
    let ring_epoch_id_str = client_config.ring_epoch_id;

    // Build protocol chain for the entry hop.
    //
    // For TCP chains: build a real `ProtocolChain` with the entry-hop camouflage so
    // `TunnelPool` can call `chain_protocols.connect(...)` on each slot.
    //
    // For UDP chains (`chain.protocol == ProtocolId::Udp(_)`): `establish_raw_tunnel`
    // in mirage-dial-core branches on `chain.protocol` and calls
    // `establish_udp_chain_tunnel` directly — it never touches `chain_protocols`.
    // We still need to pass a valid (non-empty) `ProtocolChain` to `TunnelPool::new`
    // because the API requires it, so we build a TCP placeholder; it is never used.
    let mesh_id = client_config.mesh_id.as_bytes().to_vec();
    use mirage_core::config::{AuthMode, MirageEndpointConfig, TransportCamouflage, TransportKind};
    let entry_camouflage = chain.hops.first()
        .and_then(|h| h.camouflage.clone())
        .unwrap_or(TransportCamouflage::None);
    let entry_byte_cipher = chain.hops.first().and_then(|h| h.byte_cipher.clone());

    let is_udp_chain = matches!(chain.protocol, ProtocolId::Udp(_));

    // For TCP chains build the real endpoint config with entry-hop camouflage.
    // For UDP chains build a TCP placeholder — establish_raw_tunnel exits the
    // UDP branch before ever calling chain_protocols.connect(), so this config
    // is never executed for UDP chains.
    let ep_config = MirageEndpointConfig {
        transport: TransportKind::Tcp,
        auth: AuthMode::Blake3,
        camouflage: if is_udp_chain { TransportCamouflage::None } else { entry_camouflage },
        byte_cipher: None,
        byte_cipher_mode: if is_udp_chain { None } else { entry_byte_cipher },
    };
    let proto: Box<dyn TcpProtocol> = Box::new(ProtocolEP::new(
        ep_config, TcpProtocolId::Mirage, mesh_id.clone(),
    ).with_wire_version(9));
    let chain_protocols = Arc::new(ProtocolChain::new(vec![proto]));

    if is_udp_chain {
        info!(
            chain_id = %chain_id,
            protocol = %chain.protocol,
            "UDP chain detected — TunnelPool will use establish_udp_chain_tunnel path"
        );
    }

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

    // Shared credential params arc — pre-armed from config (sentinel injects the signed
    // CredentialParams into client-config.json at materialise time so the gate clears
    // immediately on first dial, without waiting for the worker's own MonitorLink).
    let credential_params: Arc<ArcSwap<Option<mirage_core::crypto::credential::CredentialParams>>> =
        Arc::new(ArcSwap::from_pointee(client_config.credential_params.clone()));
    if let Some(ref cp) = client_config.credential_params {
        let scheme = cp.authorities.first().map(|a| format!("{:?}", a.scheme)).unwrap_or_default();
        info!(chain_id = %chain_id, scheme = %scheme, "worker: credential_params loaded from config");
    }

    // Shared blind-token pool arc (Phase 4 spend).  MonitorLink fills it; pools drain it.
    let token_pool: Arc<ArcSwap<mirage_client_core::token_pool::TokenPool>> =
        Arc::new(ArcSwap::from_pointee(mirage_client_core::token_pool::TokenPool::default()));

    // Derive the per-client shared counter directory from the config file path.
    // All workers for the same client read the same config file, so its parent
    // directory is a natural shared location for cross-process counter files.
    let nf_counter_dir = std::path::Path::new(&config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    // Ring-membership credential (scheme 0x04). Parsed once at startup; wrapped
    // in a RingCredentialCell so MonitorLink can hot-swap it on epoch rotation.
    // The persisted counter ensures concurrent/restarted workers continue the
    // monotonic nullifier sequence and never replay an index the exit has seen.
    info!(
        chain_id = %chain_id,
        counter_dir = %nf_counter_dir.display(),
        "ring credential: wiring persisted nullifier counter"
    );
    let ring_credential: mirage_client_core::token_pool::RingCredentialCell =
        Arc::new(ArcSwap::from_pointee(ring_credential_from_config(
            ring_member_sk_hex_str.as_deref(),
            ring_path_str.as_deref(),
            ring_epoch_id_str,
            &nf_counter_dir,
        )));

    // Gate initial dials when the operator key is configured: credential-gated
    // nodes require a valid credential before accepting any connection.
    let credential_gated = client_config.operator_pub_hex.is_some();

    // MuxPool requires onion reply mode: the mux exit (run_mux_server_session_wrapped_v7)
    // AEAD-wraps its replies, which OnionTunnel::recv unwraps per layer. Chains with
    // reply=raw never receive AEAD-wrapped replies — their exit byte-pumps raw bytes.
    // Creating a MuxPool for a reply=raw chain causes mux frame garbling → immediate EOF.
    // UDP chains never use mux — the mux protocol requires a reliable ordered stream.
    let chain_wants_mux = client_config.use_mux && chain.reply == ReplyMode::Onion && !is_udp_chain;
    let mux_pool_opt: Option<Arc<MuxPool>> = if chain_wants_mux {
        match MuxPool::new(
            chain.clone(),
            Arc::clone(&chain_protocols),
            pool_cfg.clone(),
            mesh_id.clone(),
            Arc::clone(&peer_table),
            Arc::clone(&credential_params),
            Arc::clone(&token_pool),
            Arc::clone(&ring_credential),
            credential_gated,
        ).await {
            Ok(mp) => {
                info!(chain_id = %chain_id, "mux pool ready");
                Some(mp)
            }
            Err(e) => {
                warn!(chain_id = %chain_id, "mux pool init failed, falling back to tunnel-pool-only: {e}");
                None
            }
        }
    } else {
        None
    };

    // Make the data-plane path explicit in the log. MUX = modern carry
    // (long-lived multiplexed carriers + circuit-breaker failover). run_simple =
    // legacy single-use TunnelPool, no mux, no failover — the cli1-era path. A
    // worker silently on the legacy path under steady load was the 2026-06 carry
    // regression (auto-chains defaulted to reply=raw); surface the boundary so it
    // can never hide again.
    if mux_pool_opt.is_some() {
        info!(chain_id = %chain_id, "data-plane: MUX path (reply=onion, use_mux=true) — multiplexed long-lived carriers");
    } else {
        let reason = if is_udp_chain {
            "udp chain (mux requires a reliable ordered stream)"
        } else if !client_config.use_mux {
            "use_mux=false in client config"
        } else if chain.reply != ReplyMode::Onion {
            "reply=raw chain — LEGACY single-use TunnelPool, no mux, no failover (flip the chain to reply=onion for the modern carry)"
        } else {
            "mux pool init failed (see prior warn) — degraded to TunnelPool-only"
        };
        warn!(chain_id = %chain_id, %reason, "data-plane: LEGACY non-mux path (run_simple) selected");
    }

    // On mux chains the warm TunnelPool is fallback-only (degraded-mode pool.lease in
    // socks5.rs); its spares are never leased by the mux data-plane, so the 120s
    // idle-eviction would churn-redial them in lockstep every ~120s (DPI-visible).
    // Disable idle eviction for the mux carrier pool; MuxPool owns its carriers'
    // lifetime via its own 600s reaper.  When MuxPool init failed (mux_pool_opt is
    // None), TunnelPool is the primary data-plane — keep the configured eviction.
    let pool_cfg_for_tunnel = if mux_pool_opt.is_some() {
        TunnelPoolConfig { idle_timeout_secs: 0, ..pool_cfg.clone() }
    } else {
        pool_cfg.clone()
    };
    let pool = TunnelPool::new(chain.clone(), chain_protocols, pool_cfg_for_tunnel, mesh_id, Arc::clone(&peer_table), Arc::clone(&credential_params), Arc::clone(&token_pool), Arc::clone(&ring_credential), credential_gated)
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

    // Per-worker kill-switch setup.
    //
    // A worker owns exactly one chain — no cross-chain failover pools are
    // available here (General-group cross-wire rerouting spans multiple workers
    // and is a supervisor-level concern, not handled inside a single worker).
    // We pass an empty failover list so the server is purely fail-closed for
    // its own wire.  The policy map contains this worker's single chain so the
    // kill-switch invariant (dead wire → SOCKS5 error, never bypass) is enforced.
    let shared_wire_health = WireHealthMap::new();
    let worker_policy = SocksFailurePolicy::from_chain_config(
        chain.kill_switch.as_ref(),
        chain.group.as_deref(),
        &chain_id,
    );
    let chain_policies: HashMap<String, SocksFailurePolicy> =
        [(chain_id.clone(), worker_policy)].into();
    let chain_natures: HashMap<String, WireNature> =
        [(chain_id.clone(), chain.effective_nature())].into();

    // Spawn SOCKS5 server.
    let server = Socks5Server::new_with_failover(addr, vec![])
        .with_kill_switch(chain_id.clone(), chain_policies, chain_natures)
        .with_shared_wire_health(Arc::clone(&shared_wire_health));
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
        Arc::clone(&credential_params),
        Arc::clone(&token_pool),
        Arc::clone(&ring_credential),
        stop_tx,
        outgoing_tx,
        outgoing_rx,
        orphan_timeout_secs,
        wire_addr_str,
        wire_pubkey_str,
        operator_pub_hex_str,
        enrol_token_str,
        nf_counter_dir,
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
    credential_params: Arc<ArcSwap<Option<mirage_core::crypto::credential::CredentialParams>>>,
    token_pool: Arc<ArcSwap<mirage_client_core::token_pool::TokenPool>>,
    ring_credential: mirage_client_core::token_pool::RingCredentialCell,
    stop_tx: watch::Sender<bool>,
    outgoing_tx: Arc<mpsc::Sender<WorkerMsg>>,
    mut outgoing_rx: mpsc::Receiver<WorkerMsg>,
    orphan_timeout_secs: u64,
    wire_addr: Option<String>,
    wire_pubkey: Option<String>,
    operator_pub_hex: Option<String>,
    enrol_token: Option<String>,
    nf_counter_dir: std::path::PathBuf,
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
            Arc::clone(&credential_params),
            Arc::clone(&token_pool),
            Arc::clone(&ring_credential),
            Arc::clone(&outgoing_tx),
            &mut outgoing_rx,
            &stop_tx,
            wire_addr.clone(),
            wire_pubkey.clone(),
            operator_pub_hex.clone(),
            enrol_token.clone(),
            nf_counter_dir.clone(),
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
    credential_params: Arc<ArcSwap<Option<mirage_core::crypto::credential::CredentialParams>>>,
    token_pool: Arc<ArcSwap<mirage_client_core::token_pool::TokenPool>>,
    ring_credential: mirage_client_core::token_pool::RingCredentialCell,
    outgoing_tx: Arc<mpsc::Sender<WorkerMsg>>,
    outgoing_rx: &mut mpsc::Receiver<WorkerMsg>,
    stop_tx: &watch::Sender<bool>,
    wire_addr: Option<String>,
    wire_pubkey: Option<String>,
    operator_pub_hex: Option<String>,
    enrol_token: Option<String>,
    nf_counter_dir: std::path::PathBuf,
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
            Ok(Some(SupervisorMsg::CredentialParamsUpdate { credential_params_json })) => {
                // Relayed from the MonitorLink host worker via the supervisor
                // (this worker has no MonitorLink of its own — see
                // `spawn_monitor_link_in_worker`'s CredentialParamsSnapshot
                // watcher). Mirrors the PeerTableUpdate arm above.
                match serde_json::from_str::<mirage_core::crypto::credential::CredentialParams>(
                    &credential_params_json,
                ) {
                    Ok(cp) => {
                        info!(
                            chain_id,
                            generation = cp.generation,
                            "worker: CredentialParamsUpdate applied"
                        );
                        credential_params.store(Arc::new(Some(cp)));
                    }
                    Err(e) => {
                        warn!(chain_id, "worker: CredentialParamsUpdate parse error: {e}");
                    }
                }
            }
            Ok(Some(SupervisorMsg::SetupMonitorLink {
                monitor_api_key,
                transports_json,
                tun_api_url,
            })) => {
                info!(chain_id, "worker: SetupMonitorLink received — spawning MonitorLink");
                spawn_monitor_link_in_worker(
                    chain_id,
                    monitor_api_key,
                    transports_json,
                    tun_api_url,
                    mesh_id.to_string(),
                    node_client_id.to_string(),
                    Arc::clone(&peer_table),
                    Arc::clone(&credential_params),
                    Arc::clone(&token_pool),
                    Arc::clone(&ring_credential),
                    Arc::clone(&pool),
                    mux_pool_opt.as_ref().map(Arc::clone),
                    chain_cfg.clone(),
                    Arc::clone(&outgoing_tx),
                    wire_addr.clone(),
                    wire_pubkey.clone(),
                    operator_pub_hex.clone(),
                    enrol_token.clone(),
                    nf_counter_dir.clone(),
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
    monitor_api_key: Option<String>,
    transports_json: String,
    tun_api_url: Option<String>,
    mesh_id: String,
    node_client_id: String,
    peer_table: Arc<ArcSwap<PeerTable>>,
    credential_params: Arc<ArcSwap<Option<mirage_core::crypto::credential::CredentialParams>>>,
    token_pool: Arc<ArcSwap<mirage_client_core::token_pool::TokenPool>>,
    ring_credential: mirage_client_core::token_pool::RingCredentialCell,
    pool: Arc<TunnelPool>,
    mux_pool_opt: Option<Arc<MuxPool>>,
    chain_cfg: mirage_core::config::ChainConfig,
    outgoing_tx: Arc<mpsc::Sender<WorkerMsg>>,
    wire_addr: Option<String>,
    wire_pubkey: Option<String>,
    operator_pub_hex: Option<String>,
    enrol_token: Option<String>,
    nf_counter_dir: std::path::PathBuf,
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
    // Wire transport from ClientConfig fields is prepended first (preferred).
    let mut transports: Vec<MonitorTransport> = Vec::new();

    // Prepend Wire from ClientConfig.monitor_wire_addr / monitor_wire_pubkey_hex
    // when both are present. This is the primary Wire source for the cli2 worker
    // path (mirrors the l0-sentinel construction in SentinelConfig).
    if let (Some(ref wa), Some(ref wp)) = (&wire_addr, &wire_pubkey) {
        info!(
            chain_id,
            addr = %wa,
            "worker: Wire transport configured from ClientConfig — prepending to MonitorLink transports"
        );
        transports.push(MonitorTransport::Wire {
            monitor_addr: wa.clone(),
            monitor_pubkey_hex: wp.clone(),
            mesh_id: mesh_id.clone(),
            host_hint: None,
        });
    }

    for spec in transport_specs {
        match spec {
            MonitorTransportSpec::DirectWs { .. } => {
                // DirectWs has been removed from MonitorTransport — skip.
                debug!(chain_id, "worker: DirectWs transport spec ignored (removed)");
            }
            MonitorTransportSpec::OverChain { worker_socks5, monitor_addr, chain_id: cid } => {
                transports.push(MonitorTransport::OverChain {
                    worker_socks5,
                    monitor_addr,
                    chain_id: ChainId(cid),
                });
            }
            MonitorTransportSpec::Wire { monitor_addr, monitor_pubkey_hex, mesh_id: mid } => {
                // Wire from supervisor transports_json — only add if not already
                // covered by the ClientConfig flat fields (avoid duplicate).
                let already_prepended = wire_addr.is_some() && wire_pubkey.is_some();
                if !already_prepended {
                    transports.push(MonitorTransport::Wire {
                        monitor_addr,
                        monitor_pubkey_hex,
                        mesh_id: mid,
                        host_hint: None,
                    });
                }
            }
        }
    }

    // Blind-token credential wiring (mirrors cli + l0-sentinel): verify pushed
    // CredentialParams against the operator key when configured, arm blind-RSA
    // batch requests with the enrol token. Bad operator_pub_hex → log + unverified.
    let operator_pub = operator_pub_hex.as_deref().and_then(|hex_str| {
        match MonitorLink::parse_operator_pub_hex(hex_str) {
            Ok(k) => {
                info!(chain_id, "worker: operator_pub_hex decoded — CredentialParams will be verified");
                Some(k)
            }
            Err(e) => {
                warn!(chain_id, "worker: invalid operator_pub_hex ({e}); CredentialParams applied unverified");
                None
            }
        }
    });

    let (monitor_link, mut config_rx) = MonitorLink::new_with_credential_arc(
        monitor_api_key,
        Arc::clone(&peer_table),
        Arc::clone(&credential_params),
        Arc::clone(&token_pool),
        Arc::clone(&ring_credential),
        chain_set,
        transports,
        mesh_id,
        node_client_id,
        operator_pub,
    );
    monitor_link.set_enrol_token(enrol_token);
    monitor_link.set_nf_counter_dir(nf_counter_dir);
    monitor_link.spawn();

    // Policy-push task: forward monitor-pushed SplitPolicyConfig to the local
    // TUN daemon on every config version bump.  Only spawned when tun_api_url
    // is set (SOCKS-only mode has no TUN to push to).
    if let Some(u) = tun_api_url {
        tokio::spawn(mirage_client_core::policy_push::run_policy_pusher(config_rx.clone(), u));
    } else {
        info!(chain_id, "worker: no tun_api_url — policy-push task not started (SOCKS-only mode)");
    }

    // Watcher task: on each pushed `CredentialParams` frame, forward the
    // current authority set to the supervisor via `outgoing_tx` as
    // `WorkerMsg::CredentialParamsSnapshot`.
    //
    // WHY a separate watcher (not the peer_table `config_rx` one below):
    // `apply_credential_params` (monitor_link.rs) only pushes through
    // `frame_tx`, never `config_tx`, so `config_rx.changed()` never fires
    // for a credential-only push. Multi-wire clients run N sibling workers
    // but only ONE ever hosts a MonitorLink (`monitor_link_host=true`);
    // without this relay, sibling workers never see CredentialParams and
    // dial with credential_scheme=0x00 forever. Mirrors the peer_table
    // broadcast path (`WorkerMsg::PeerTableSnapshot` /
    // `SupervisorMsg::PeerTableUpdate`) 1:1, applied to the credential
    // authority set instead of the mesh peer table.
    {
        let mut cred_frame_rx = monitor_link.subscribe_frames();
        let credential_params_for_watch = Arc::clone(&credential_params);
        let outgoing_tx_cred = Arc::clone(&outgoing_tx);
        let chain_id_owned_cred = chain_id.to_string();
        tokio::spawn(async move {
            let mut last_generation: u64 = 0;
            loop {
                if cred_frame_rx.changed().await.is_err() {
                    // MonitorLink dropped — watcher exits.
                    break;
                }
                let frame = cred_frame_rx.borrow().clone();
                if !matches!(frame, Some(WsServerFrame::CredentialParams(_))) {
                    continue;
                }
                // Read the current params directly from the ArcSwap (already
                // updated by MonitorLink::apply_credential_params before the
                // watch fires) — same pattern as the peer_table watcher below.
                let snapshot = credential_params_for_watch.load();
                let Some(ref params) = **snapshot else { continue };
                let generation = params.generation;
                if generation <= last_generation && last_generation != 0 {
                    continue;
                }
                last_generation = generation;
                match serde_json::to_string(params) {
                    Ok(json) => {
                        debug!(
                            chain_id = %chain_id_owned_cred,
                            generation,
                            "worker: sending CredentialParamsSnapshot to supervisor"
                        );
                        let msg = WorkerMsg::CredentialParamsSnapshot {
                            generation,
                            credential_params_json: json,
                        };
                        if outgoing_tx_cred.send(msg).await.is_err() {
                            break; // supervisor disconnected — stop
                        }
                    }
                    Err(e) => {
                        warn!(chain_id = %chain_id_owned_cred, "worker: credential_params serialize error: {e}");
                    }
                }
            }
        });
    }

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

/// Parse ring-membership credential material from config fields, if present
/// and well-formed. Returns None (with a warn) if any field is missing/invalid —
/// the worker then dials without a 0x04 credential.
///
/// `counter_dir` is the directory where the per-(member, epoch) nullifier
/// counter file is stored. Using the config file's parent ensures all concurrent
/// and restarted workers for the same client share a single monotonic counter.
fn ring_credential_from_config(
    sk_hex: Option<&str>,
    path_cfg: Option<&[mirage_core::config::RingPathStep]>,
    epoch_id: Option<u32>,
    counter_dir: &std::path::Path,
) -> Option<mirage_client_core::RingCredential> {
    let sk_hex = sk_hex?;
    let path_cfg = path_cfg?;
    let epoch_id = epoch_id?;
    let sk_bytes: [u8; 32] = hex::decode(sk_hex)
        .ok()?
        .try_into()
        .ok()?;
    let member_sk = mirage_ringmember::MemberSecret::from_bytes(sk_bytes).or_else(|| {
        warn!("ring_credential_from_config: ring_member_sk_hex is not a canonical ristretto scalar — ring credential disabled");
        None
    })?;
    let mut path = Vec::with_capacity(path_cfg.len());
    for step in path_cfg {
        let sib: [u8; 32] = hex::decode(&step.sibling_hex)
            .ok()?
            .try_into()
            .ok()?;
        path.push((sib, step.is_right));
    }
    Some(mirage_client_core::RingCredential::new_persisted(member_sk, path, epoch_id, counter_dir))
}
