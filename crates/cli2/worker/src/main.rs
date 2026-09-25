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
    MonitorTransportSpec, Socks5ListenSpec, SupervisorMsg, WorkerMsg, WorkerState,
    pid_file_path, pipe_path,
    pipe::{PipePair, PipeListener},
};
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use mirage_client_core::{
    ChainHandle, ChainId, ChainMetrics, ChainSet, ChainState, HealthProbeConfig, PrewarmStrategy,
    monitor_link::{MonitorLink, MonitorLinkOptions, MonitorTransport, WsServerFrame},
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

/// Exit code used when the worker gives up waiting for the supervisor to
/// reconnect (orphan timeout), or the IPC listener itself fails while
/// orphaned. Distinct from a clean exit (`0` — Drain/Shutdown/Restart, all
/// requested by the supervisor) and from a panic (`1`, see the panic hook
/// below) so a supervising parent can tell "nobody came back" apart from
/// "asked to stop" and respawn the pair accordingly.
const EXIT_CODE_ORPHAN_TIMEOUT: i32 = 3;

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
    let (socks5_listen, config_path, initial_peer_table_json, initial_ipc) =
        wait_for_start(&mut listener, &chain_id, pid, started_at_unix, WorkerState::Starting).await?;

    // Validate once, here, so a malformed Start fails the worker fast and
    // loudly rather than surfacing later as an opaque bind error deep inside
    // `spawn_socks5_accept_loop`.
    match &socks5_listen {
        Socks5ListenSpec::Tcp { addr } => {
            addr.parse::<SocketAddr>()
                .with_context(|| format!("invalid socks5_listen TCP addr '{addr}'"))?;
        }
        Socks5ListenSpec::Unix { path } => {
            if path.is_empty() {
                anyhow::bail!("socks5_listen unix path is empty");
            }
        }
    }

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

    // Pre-minted SignedCapability (0x02) token source. The client cannot
    // self-mint 0x02 (only the operator-box holds the Ed25519 seed), so tokens
    // come from a file provisioned externally (test line; the source is a
    // replaceable seam — see capability_pool). No reload wiring needed: every
    // pop lazily re-reads the file, so tokens appended later are picked up.
    let capability_tokens: mirage_client_core::capability_pool::CapabilityTokenCell =
        match client_config.capability_tokens_path.as_deref() {
            Some(path) => {
                info!(chain_id = %chain_id, path, "worker: capability token file source configured");
                Arc::new(mirage_client_core::capability_pool::FileCapabilityTokenSource::new(path))
            }
            None => mirage_client_core::capability_pool::null_capability_cell(),
        };

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
            Arc::clone(&capability_tokens),
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

    let pool_cfg_for_tunnel = tunnel_pool_config_for(&pool_cfg, mux_pool_opt.is_some());
    // Fallback-only pools cost nothing until something actually leases one: every
    // slot starts Dormant and `lease()` wakes it. When MuxPool init failed this
    // pool IS the data plane, so it stays eager — a first CONNECT must not wait
    // on a cold dial.
    let pool = if mux_pool_opt.is_some() {
        TunnelPool::new_lazy(chain.clone(), chain_protocols, pool_cfg_for_tunnel, mesh_id, Arc::clone(&peer_table), Arc::clone(&credential_params), Arc::clone(&token_pool), Arc::clone(&ring_credential), Arc::clone(&capability_tokens), credential_gated)
            .await
            .map_err(|e| anyhow::anyhow!("TunnelPool::new_lazy failed: {e}"))?
    } else {
        TunnelPool::new(chain.clone(), chain_protocols, pool_cfg_for_tunnel, mesh_id, Arc::clone(&peer_table), Arc::clone(&credential_params), Arc::clone(&token_pool), Arc::clone(&ring_credential), Arc::clone(&capability_tokens), credential_gated)
            .await
            .map_err(|e| anyhow::anyhow!("TunnelPool::new failed: {e}"))?
    };

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

    // Pulse channel used to stop whichever SOCKS5 accept loop is currently
    // running. Fired from `handle_session` on Drain/Shutdown/Restart, and
    // from `supervisor_loop` on an ordinary supervisor disconnect (the fix
    // for the orphan-window blackhole — see `stop_socks5` /
    // `spawn_socks5_accept_loop` below). Each accept-loop spawn subscribes a
    // fresh receiver via `stop_tx.subscribe()`, so reusing one sender across
    // many stop/resume cycles is safe — a receiver only observes `.send()`
    // calls made after it subscribed.
    let (stop_tx, _) = watch::channel(false);

    let current_state = Arc::new(tokio::sync::Mutex::new(WorkerState::Ready));

    // Channel for background tasks (MonitorLink watcher) to send WorkerMsg
    // back to the supervisor via the IPC pipe. The receive end lives inside
    // `supervisor_loop` / `handle_session`.
    let (outgoing_tx_raw, outgoing_rx) = mpsc::channel::<WorkerMsg>(16);
    let outgoing_tx = Arc::new(outgoing_tx_raw);

    // IPC supervisor loop — handles current session + reconnects. Also owns
    // the SOCKS5 accept-loop lifecycle: live only while a supervisor is
    // connected, torn down the instant the link is lost so new dials get an
    // honest connection-refused instead of being accepted and blackholed for
    // up to `orphan_timeout_secs`.
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
        mux_pool_opt.as_ref().map(Arc::clone),
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
        socks5_listen,
        config_arc,
        router,
    )
    .await;

    // Remove PID file on clean exit.
    if let Err(e) = std::fs::remove_file(&pid_file) {
        warn!("could not remove PID file {:?}: {e}", pid_file);
    }

    match result {
        Ok(WorkerExit::Clean) => Ok(()),
        Ok(WorkerExit::Orphaned) => {
            warn!(
                chain_id = %chain_id,
                exit_code = EXIT_CODE_ORPHAN_TIMEOUT,
                "worker exiting via the orphan-timeout path — supervisor never reconnected, signalling a supervising parent to respawn the pair"
            );
            std::process::exit(EXIT_CODE_ORPHAN_TIMEOUT);
        }
        Err(e) => Err(e),
    }
}

/// Derive the `TunnelPoolConfig` for the worker's fallback `TunnelPool`,
/// shrinking it when the mux carrier pool is healthy.
///
/// When `mux_is_alive`, `TunnelPool` is fallback-only (degraded-mode
/// `pool.lease()` call sites in socks5.rs) — its spares are never leased by
/// the mux data-plane. Two changes apply in that case:
/// - `pool_size` floors to 1: building it at the same pool_size the mux
///   carrier pool uses (3, forced under `use_mux`) would eagerly dial 3 more
///   tunnels — 3 more capability-token spends — at cold start for a pool
///   nothing but the fallback path ever leases while mux is healthy.
///   `TunnelPool::new` floors pool_size at 1 regardless of what's passed, so
///   1 is the cheapest fallback that still exists to catch a runtime mux
///   failure.
/// - `idle_timeout_secs` disables (0): the 120s idle-eviction would
///   churn-redial the fallback spares in lockstep every ~120s (DPI-visible).
///   MuxPool owns its own carriers' lifetime via its own 600s reaper.
///
/// When mux init failed (`!mux_is_alive`), `TunnelPool` is the primary
/// data-plane — `pool_cfg` passes through unchanged.
fn tunnel_pool_config_for(pool_cfg: &TunnelPoolConfig, mux_is_alive: bool) -> TunnelPoolConfig {
    if mux_is_alive {
        TunnelPoolConfig { idle_timeout_secs: 0, pool_size: 1, ..pool_cfg.clone() }
    } else {
        pool_cfg.clone()
    }
}

/// Run the IPC command loop across multiple supervisor connections.
///
/// Also owns the SOCKS5 accept-loop lifecycle for this chain: a fresh accept
/// loop is spawned as soon as a supervisor session is live, and torn down
/// (listener dropped — OS refuses new connections) the instant the link is
/// lost, for the whole orphan wait. This is what bounds the blackhole window
/// to zero: `orphan_timeout_secs` now only bounds how long the worker keeps
/// *refusing* dials while waiting for the supervisor to come back, not how
/// long it silently accepts-and-hangs them.
///
/// Returns `Ok(WorkerExit::Clean)` when the worker should exit cleanly
/// (Drain completed or Restart acknowledged — the supervisor asked for it).
/// Returns `Ok(WorkerExit::Orphaned)` when no supervisor reconnected within
/// `orphan_timeout_secs`, or the IPC listener itself failed while waiting —
/// `main` maps this to a distinct non-zero exit code. Returns `Err` on fatal
/// internal errors.
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
    socks5_listen: Socks5ListenSpec,
    socks5_config: Arc<ClientConfig>,
    socks5_router: Arc<RoutingTable>,
) -> anyhow::Result<WorkerExit> {
    let mut ipc = initial_ipc;

    // Per-worker kill-switch setup, recomputed on every accept-loop (re)spawn
    // inside `start_socks5` below.
    //
    // A worker owns exactly one chain — no cross-chain failover pools are
    // available here (General-group cross-wire rerouting spans multiple
    // workers and is a supervisor-level concern, not handled inside a single
    // worker). `wire_health` persists across accept-loop stop/resume cycles
    // (orphan pauses) so kill-switch failure streaks survive a brief
    // supervisor blip instead of resetting on every reconnect.
    let shared_wire_health = WireHealthMap::new();

    // Builds (or rebuilds) the SOCKS5 accept loop for the current pool/mux
    // pair. Only the stop-signal receiver changes between calls — pool,
    // mux_pool_opt, and the socks5_* config are fixed for the worker's
    // lifetime.
    let start_socks5 = || {
        spawn_socks5_accept_loop(
            socks5_listen.clone(),
            chain_id,
            chain_cfg,
            Arc::clone(&shared_wire_health),
            Arc::clone(&pool),
            mux_pool_opt.as_ref().map(Arc::clone),
            Arc::clone(&socks5_config),
            Arc::clone(&socks5_router),
            stop_tx.subscribe(),
        )
    };

    let mut server_task: Option<tokio::task::JoinHandle<()>> = Some(start_socks5());

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
                stop_socks5(&mut server_task, &stop_tx).await;
                return Ok(WorkerExit::Clean);
            }
            SessionEnd::Disconnect => {
                // Supervisor disconnected. Stop accepting SOCKS5 connections
                // right now — the orphan wait below bounds how long dials are
                // REFUSED (listener down), never how long they're accepted
                // and then blackholed.
                info!(chain_id, "supervisor disconnected — SOCKS5 accept loop stopped, waiting for reconnect");
                stop_socks5(&mut server_task, &stop_tx).await;
            }
        }

        // Orphan wait: accept next supervisor connection or timeout. The
        // SOCKS5 listener stays down for the entire wait.
        let orphan_timeout = tokio::time::Duration::from_secs(orphan_timeout_secs);
        match tokio::time::timeout(orphan_timeout, listener.accept()).await {
            Err(_) => {
                warn!(
                    chain_id,
                    timeout_secs = orphan_timeout_secs,
                    "no supervisor reconnected — orphan timeout, exiting"
                );
                return Ok(WorkerExit::Orphaned);
            }
            Ok(Err(e)) => {
                warn!(chain_id, "IPC accept error: {e} — exiting");
                return Ok(WorkerExit::Orphaned);
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
                        server_task = Some(start_socks5());
                    }
                    Ok(Some(SupervisorMsg::Restart { reason })) => {
                        info!(chain_id, reason = %reason, "supervisor sent Restart — draining");
                        let _ = stop_tx.send(true);
                        return Ok(WorkerExit::Clean);
                    }
                    Ok(Some(other)) => {
                        warn!(chain_id, ?other, "unexpected message after Hello — treating as Continue");
                        server_task = Some(start_socks5());
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

/// Outcome of [`supervisor_loop`], distinguishing a clean stop the supervisor
/// asked for (Drain/Shutdown/Restart) from giving up on an orphan (nobody
/// reconnected within `orphan_timeout_secs`, or the IPC listener itself
/// failed while waiting). `main` maps `Orphaned` to a distinct non-zero exit
/// code so a supervising parent can respawn the pair instead of treating this
/// exit the same as a requested stop.
enum WorkerExit {
    /// Drain/Shutdown completed, or Restart was acknowledged.
    Clean,
    /// No supervisor reconnected within the orphan window (or the IPC
    /// listener failed while waiting).
    Orphaned,
}

/// Tear down the currently running SOCKS5 accept loop, if any: pulse
/// `stop_tx` and wait for the accept-loop task to unwind. Unwinding drops the
/// bound `TcpListener`, so the OS refuses new connection attempts instead of
/// the process accepting them and then blackholing the client. Idempotent —
/// a second call with `task` already `None` is a no-op.
async fn stop_socks5(task: &mut Option<tokio::task::JoinHandle<()>>, stop_tx: &watch::Sender<bool>) {
    if let Some(handle) = task.take() {
        let _ = stop_tx.send(true);
        let _ = handle.await;
    }
}

/// Spawn a fresh SOCKS5 accept loop for this chain, bound to `listen`.
///
/// `listen` selects the transport: `Tcp` binds a loopback port (workstation
/// mode, unchanged), `Unix` binds a socket in the edge-client's own private
/// runtime directory (box mode — never a TCP port, owner ruling
/// 2026-09-23). The SOCKS5 protocol served is identical either way; only
/// which `Socks5Server` method runs the accept loop differs.
///
/// Returns the loop's `JoinHandle`. The caller stops it by pulsing any
/// `watch::Sender` that `stop_rx` was subscribed from (see [`stop_socks5`]) —
/// the loop races the accept future against `stop_rx.changed()`, aborts the
/// accept future on a pulse, and exits. `stop_rx` must be freshly subscribed
/// (`stop_tx.subscribe()`) for each call: a receiver only observes `.send()`
/// calls made *after* it subscribed, so reusing an old receiver across a
/// stop/respawn cycle would make the new loop see a pulse instantly.
fn spawn_socks5_accept_loop(
    listen: Socks5ListenSpec,
    chain_id: &str,
    chain_cfg: &mirage_core::config::ChainConfig,
    shared_wire_health: Arc<WireHealthMap>,
    pool: Arc<TunnelPool>,
    mux_pool_opt: Option<Arc<MuxPool>>,
    config_arc: Arc<ClientConfig>,
    router: Arc<RoutingTable>,
    mut stop_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    // Per-worker kill-switch setup. A worker owns exactly one chain — no
    // cross-chain failover pools are available here (General-group
    // cross-wire rerouting spans multiple workers and is a supervisor-level
    // concern, not handled inside a single worker). We pass an empty
    // failover list so the server is purely fail-closed for its own wire.
    // The policy map contains this worker's single chain so the kill-switch
    // invariant (dead wire → SOCKS5 error, never bypass) is enforced.
    let worker_policy = SocksFailurePolicy::from_chain_config(
        chain_cfg.kill_switch.as_ref(),
        chain_cfg.group.as_deref(),
        chain_id,
    );
    let chain_policies: HashMap<String, SocksFailurePolicy> =
        [(chain_id.to_string(), worker_policy)].into();
    let chain_natures: HashMap<String, WireNature> =
        [(chain_id.to_string(), chain_cfg.effective_nature())].into();

    // `Socks5Server::new_with_failover` always wants a `SocketAddr` field —
    // it is genuinely used to bind in `Tcp` mode; in `Unix` mode it is dead
    // weight (`run_unix`/`run_simple_unix` never read `self.bind_addr`), so a
    // loopback placeholder is fine there.
    let bind_addr: SocketAddr = match &listen {
        Socks5ListenSpec::Tcp { addr } => addr
            .parse()
            .expect("socks5_listen Tcp addr was already validated as parseable in main() before spawn_socks5_accept_loop was ever called"),
        Socks5ListenSpec::Unix { .. } => SocketAddr::from(([127, 0, 0, 1], 0)),
    };

    let server = Socks5Server::new_with_failover(bind_addr, vec![])
        .with_kill_switch(chain_id.to_string(), chain_policies, chain_natures)
        .with_shared_wire_health(shared_wire_health);

    let chain_id = chain_id.to_string();
    let mux_pool_is_some = mux_pool_opt.is_some();
    tokio::spawn(async move {
        let mut serve_fut: tokio::task::JoinHandle<Result<(), mirage_core::error::MirageError>> =
            match listen {
                Socks5ListenSpec::Tcp { .. } => {
                    if mux_pool_is_some {
                        tokio::spawn(async move { server.run(pool, mux_pool_opt, config_arc, router).await })
                    } else {
                        tokio::spawn(async move { server.run_simple(pool).await })
                    }
                }
                #[cfg(unix)]
                Socks5ListenSpec::Unix { path } => {
                    let path = std::path::PathBuf::from(path);
                    if mux_pool_is_some {
                        tokio::spawn(async move { server.run_unix(&path, pool, mux_pool_opt, config_arc, router).await })
                    } else {
                        tokio::spawn(async move { server.run_simple_unix(&path, pool).await })
                    }
                }
                // Box mode only ever runs on the fleet's Linux boxes. This
                // arm exists so the worker still compiles on the Windows
                // dev/test target — it is never reached there, since
                // nothing on Windows sends a `Socks5ListenSpec::Unix` Start.
                #[cfg(not(unix))]
                Socks5ListenSpec::Unix { .. } => {
                    let _ = (pool, mux_pool_opt, config_arc, router);
                    tokio::spawn(async move {
                        Err(mirage_core::error::MirageError::Config(
                            "box-mode unix SOCKS5 listener requested on a non-unix target".to_string(),
                        ))
                    })
                }
            };
        tokio::select! {
            result = &mut serve_fut => {
                if let Ok(Err(e)) = result {
                    tracing::error!(chain_id = %chain_id, error = %e, "SOCKS5 server exited with error");
                }
            }
            _ = stop_rx.changed() => {
                serve_fut.abort();
                let _ = serve_fut.await;
                debug!(chain_id = %chain_id, "SOCKS5 accept loop stopped — no longer accepting connections");
            }
        }
    })
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
        MonitorLinkOptions {
            operator_pub,
            // The worker never spawns a debug shell (only l0-sentinel does) —
            // it has no business honouring this frame either way.
            allow_operator_debug_shell: false,
        },
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
/// Returns `(socks5_listen, config_path, peer_table_json, ipc)`.
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
) -> anyhow::Result<(Socks5ListenSpec, String, Option<String>, PipePair)> {
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
                    socks5_listen,
                    config_path,
                    peer_table_json,
                }) => {
                    info!(chain_id, listen = ?socks5_listen, "received Start");
                    // Prefer peer_table_json from Start; fall back to any
                    // PeerTableUpdate that arrived before Start.
                    let resolved_pt = peer_table_json.or(buffered_peer_table_json);
                    return Ok((socks5_listen, config_path, resolved_pt, ipc));
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Defect 1: fallback TunnelPool sizing ────────────────────────────────

    #[test]
    fn tunnel_pool_config_for_mux_alive_floors_pool_size_and_disables_eviction() {
        let base = TunnelPoolConfig { pool_size: 3, idle_timeout_secs: 120, ..Default::default() };

        let derived = tunnel_pool_config_for(&base, true);

        assert_eq!(derived.pool_size, 1, "healthy mux must not pay for 3 eager fallback dials");
        assert_eq!(derived.idle_timeout_secs, 0, "mux path disables fallback idle eviction");
    }

    #[test]
    fn tunnel_pool_config_for_mux_dead_passes_through_unchanged() {
        let base = TunnelPoolConfig { pool_size: 3, idle_timeout_secs: 120, ..Default::default() };

        let derived = tunnel_pool_config_for(&base, false);

        assert_eq!(derived.pool_size, base.pool_size, "TunnelPool is primary here — keep the configured size");
        assert_eq!(derived.idle_timeout_secs, base.idle_timeout_secs, "TunnelPool is primary here — keep the configured eviction");
    }

    // ── Defect 2: SOCKS5 accept-loop stop/resume signalling ─────────────────
    //
    // These tests cover the pause/resume *signalling contract* that the
    // orphan-window fix depends on: `stop_socks5` must actually stop a
    // running accept-loop task (idempotently), and a freshly-subscribed
    // `watch::Receiver` must not fire on a pulse sent before it subscribed —
    // otherwise every resumed accept loop would see a stale "stop" and never
    // actually start accepting again. They exercise the real `stop_socks5`
    // against a stand-in task shaped exactly like the real accept-loop's
    // `tokio::select!` (pending future raced against `stop_rx.changed()`).
    //
    // NOT covered here (not unit-testable from this crate): that
    // `spawn_socks5_accept_loop` actually stops accepting on a real
    // `TcpListener`, or that a real `Socks5Server` refuses dials while
    // stopped. That needs a live-bound listener plus a real
    // `TunnelPool`/`MuxPool`/`ClientConfig`/`RoutingTable` fixture — the
    // worker crate is a bin-only crate with no test-fixture scaffolding for
    // those types, and building one here would duplicate most of `main()`
    // without adding confidence beyond what the signalling contract already
    // proves.

    /// Stand-in for the real accept-loop task's `tokio::select!` shape,
    /// without a real `Socks5Server`/`TunnelPool`.
    fn spawn_stub_accept_loop(mut stop_rx: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tokio::select! {
                _ = std::future::pending::<()>() => {}
                _ = stop_rx.changed() => {}
            }
        })
    }

    #[tokio::test]
    async fn stop_socks5_stops_the_running_task() {
        let (stop_tx, _) = watch::channel(false);
        let mut task = Some(spawn_stub_accept_loop(stop_tx.subscribe()));

        stop_socks5(&mut task, &stop_tx).await;

        assert!(task.is_none(), "stop_socks5 must clear the handle of the task it stopped");
    }

    #[tokio::test]
    async fn stop_socks5_is_idempotent_when_already_stopped() {
        let (stop_tx, _) = watch::channel(false);
        let mut task: Option<tokio::task::JoinHandle<()>> = None;

        // supervisor_loop calls stop_socks5 unconditionally on every
        // disconnect, including two disconnects in a row with no successful
        // reconnect in between — must not panic or hang on an already-empty
        // handle.
        stop_socks5(&mut task, &stop_tx).await;

        assert!(task.is_none());
    }

    #[tokio::test]
    async fn fresh_subscribe_after_a_pulse_does_not_fire_immediately() {
        let (stop_tx, _) = watch::channel(false);

        // First cycle: stop it once — this pulses the shared sender.
        let mut first = Some(spawn_stub_accept_loop(stop_tx.subscribe()));
        stop_socks5(&mut first, &stop_tx).await;

        // Second cycle: a fresh receiver subscribed AFTER that pulse must
        // treat the sender's current value as already-seen and keep running
        // until the NEXT pulse. Yield repeatedly instead of sleeping — this
        // gives the spawned task every chance to wake and finish if it saw
        // a stale pulse, without a wall-clock-dependent assertion.
        let second = spawn_stub_accept_loop(stop_tx.subscribe());
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !second.is_finished(),
            "a freshly-subscribed receiver must not observe a pulse sent before it subscribed"
        );

        // A genuine pulse sent after subscribing must still stop it.
        let mut second = Some(second);
        stop_socks5(&mut second, &stop_tx).await;
        assert!(second.is_none());
    }
}
