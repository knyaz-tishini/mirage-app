//! mirage-chain-supervisor — orchestrator process.
//!
//! Reads config, discovers live workers (pipe reattach), spawns missing ones,
//! handles IPC, polls for OTA updates, and performs rolling binary rollouts.

mod config;
mod control_endpoint;
mod ota_poller;
mod reattach;
mod reconcile;
mod rollout;
mod router_listener;
mod state;
mod tun_registry;
mod worker_handle;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use config::Config;
use control_endpoint::spawn_control_endpoint;
use mirage_chain_ipc::{MonitorTransportSpec, WorkerMsg};
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use ota_poller::{UpdateAvailable, spawn_ota_poller};
use reattach::discover_orphans;
use reconcile::{reconcile, try_reload_config};
use rollout::{download_new_binary, rolling_update, shutdown_all};
use tun_registry::push_tun_registry;
use worker_handle::{HandleState, WorkerHandle};

/// Resolved SetupMonitorLink parameters ready to send to the ML-host worker.
///
/// Stored after the initial send so the respawn path can re-send without
/// reconstructing from config (config may change between boot and respawn).
#[derive(Clone)]
struct MlSetup {
    /// chain_id of the designated ML-host worker.
    host_chain_id: String,
    monitor_urls: Vec<String>,
    monitor_api_key: Option<String>,
    transports_json: String,
    /// Forwarded from `Config.tun_api_url` so the worker can push split
    /// policies to the local TUN daemon.
    tun_api_url: Option<String>,
}

/// Build the `MlSetup` struct from the current `Config`.
///
/// Returns `None` when:
/// - No chain has `monitor_link_host = true`.
/// - Transport serialisation fails (logged as `warn`).
fn build_ml_setup(config: &Config) -> Option<MlSetup> {
    use config::MonitorTransportConfig;

    let host_chain = config.chains.iter().find(|c| c.monitor_link_host)?;
    let host_chain_id = host_chain.id.clone();

    let mut ipc_specs: Vec<MonitorTransportSpec> = Vec::new();

    // Wire — flat fields take precedence.
    if let (Some(wire_addr), Some(wire_pubkey)) = (
        config.monitor_wire_addr.clone(),
        config.monitor_wire_pubkey_hex.clone(),
    ) {
        ipc_specs.push(MonitorTransportSpec::Wire {
            monitor_addr: wire_addr,
            monitor_pubkey_hex: wire_pubkey,
            mesh_id: config
                .monitor_transports
                .iter()
                .find_map(|t| {
                    if let MonitorTransportConfig::Wire { mesh_id, .. } = t {
                        Some(mesh_id.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "v3-staging".to_string()),
        });
    }

    for t in &config.monitor_transports {
        match t {
            MonitorTransportConfig::DirectWs { .. } => {}
            MonitorTransportConfig::OverChain { chain_id, monitor_addr } => {
                let chain_cfg = config.chains.iter().find(|c| &c.id == chain_id)?;
                let socks5: std::net::SocketAddr = match chain_cfg.socks5_bind.parse() {
                    Ok(a) => a,
                    Err(e) => {
                        warn!(
                            "supervisor: build_ml_setup: chain {chain_id} socks5_bind \
                             invalid: {e} — MonitorLink setup skipped"
                        );
                        return None;
                    }
                };
                ipc_specs.push(MonitorTransportSpec::OverChain {
                    worker_socks5: socks5,
                    monitor_addr: *monitor_addr,
                    chain_id: chain_id.clone(),
                });
            }
            MonitorTransportConfig::Wire { monitor_addr, monitor_pubkey_hex, mesh_id } => {
                let already = config.monitor_wire_addr.is_some()
                    && config.monitor_wire_pubkey_hex.is_some();
                if !already {
                    ipc_specs.push(MonitorTransportSpec::Wire {
                        monitor_addr: monitor_addr.clone(),
                        monitor_pubkey_hex: monitor_pubkey_hex.clone(),
                        mesh_id: mesh_id.clone(),
                    });
                }
            }
        }
    }

    let transports_json = match serde_json::to_string(&ipc_specs) {
        Ok(j) => j,
        Err(e) => {
            warn!("supervisor: build_ml_setup: serialise transports failed: {e}");
            return None;
        }
    };

    Some(MlSetup {
        host_chain_id,
        monitor_urls: vec![config.monitor_url.clone()],
        monitor_api_key: config.monitor_api_key.clone(),
        transports_json,
        tun_api_url: config.tun_api_url.clone(),
    })
}

/// Fetch the peer_table JSON from monitor's `/relay/clients/config` on cold start.
async fn bootstrap_peer_table_http(
    monitor_url: &str,
    monitor_api_key: Option<&str>,
) -> anyhow::Result<String> {
    let url = format!("{}/relay/clients/config", monitor_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let mut req = client.get(&url);
    if let Some(key) = monitor_api_key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?.error_for_status()?;
    let val: serde_json::Value = resp.json().await?;
    let pt = val
        .get("peer_table")
        .ok_or_else(|| anyhow::anyhow!("missing peer_table field in monitor response"))?;
    Ok(serde_json::to_string(pt)?)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("MIRAGE_LOG")
                .add_directive(
                    "mirage_chain_supervisor=debug".parse().expect("valid directive"),
                )
                // MonitorLink state transitions only (subscribe/WS error class/
                // OverChain switch/DirectWs restore/peer-gen). No payload, no
                // peer_table dumps, no frame bytes. Safe to keep on by default;
                // operator can drop further via MIRAGE_LOG=warn for ZT lockdown.
                .add_directive(
                    "mirage_client_core::monitor_link=info".parse().expect("valid directive"),
                ),
        )
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

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "supervisor.toml".to_string());

    let mut config = Config::load_any(&config_path)
        .with_context(|| format!("loading config from {config_path}"))?;

    // If no explicit reload_config_path, default to the CLI config path so the
    // operator can trigger chain adoption simply by editing supervisor.toml.
    if config.reload_config_path.is_none() {
        config.reload_config_path = Some(config_path.clone());
    }

    info!(
        chains = config.chains.len(),
        monitor_url = %config.monitor_url,
        "supervisor starting"
    );

    // ── Memory guard ──────────────────────────────────────────────────────────
    // On fatal: send Shutdown to all workers then exit so the parent respawns.
    let (fatal_tx, mut fatal_rx) = tokio::sync::mpsc::channel::<()>(1);
    let _memory_guard = MemoryGuard::spawn(
        MemoryGuardConfig::default(),
        Box::new(move || {
            let _ = fatal_tx.try_send(());
        }),
    );

    let binary_path = std::path::PathBuf::from(&config.worker_binary);

    // ── Phase 1: discover orphan workers from previous supervisor run ─────────
    let temp_dir = std::env::temp_dir();
    let orphans = discover_orphans(&temp_dir).await;
    let mut orphan_map: HashMap<String, _> = orphans
        .into_iter()
        .map(|o| (o.chain_id.clone(), o))
        .collect();

    if !orphan_map.is_empty() {
        info!(
            count = orphan_map.len(),
            "discovered orphan workers — reattaching"
        );
    }

    // ── Resolve initial peer_table for cold-start worker bootstrap ───────────
    let state_dir_path: PathBuf = match &config.state_dir {
        Some(s) => PathBuf::from(s),
        None => std::env::temp_dir().join("mirage-cli2-state"),
    };
    let initial_pt: Option<String> = match state::load_peer_table(&state_dir_path) {
        Some(s) => {
            info!("supervisor: loaded peer_table from {}", state_dir_path.display());
            Some(s)
        }
        None => match bootstrap_peer_table_http(
            &config.monitor_url,
            config.monitor_api_key.as_deref(),
        )
        .await
        {
            Ok(s) => {
                info!(
                    "supervisor: bootstrapped peer_table via HTTP from {}",
                    config.monitor_url
                );
                Some(s)
            }
            Err(e) => {
                warn!(
                    "supervisor: cold-start peer_table bootstrap failed (file+HTTP) — \
                     workers will start with empty pool: {e:#}"
                );
                None
            }
        },
    };

    // ── Phase 2: for each chain, reattach or spawn ───────────────────────────
    let mut handles: HashMap<String, WorkerHandle> = HashMap::new();

    for chain in &config.chains {
        if let Some(orphan) = orphan_map.remove(&chain.id) {
            info!(chain_id = %chain.id, pid = orphan.pid, "reattaching to orphan worker");
            let mut handle = WorkerHandle::reattach(orphan.chain_id, orphan.pid, orphan.ipc);
            if let Err(e) = handle.send_continue().await {
                warn!(chain_id = %chain.id, "send_continue failed: {e:#} — will respawn");
                // Fall through to spawning a fresh worker below.
                match WorkerHandle::spawn(&binary_path, chain) {
                    Err(se) => {
                        error!(chain_id = %chain.id, "respawn failed: {se:#}");
                        continue;
                    }
                    Ok(mut new_handle) => {
                        if let Err(se) = new_handle.send_start(chain, initial_pt.clone()).await {
                            error!(chain_id = %chain.id, "send_start failed: {se:#}");
                        }
                        handles.insert(chain.id.clone(), new_handle);
                    }
                }
            } else {
                handles.insert(chain.id.clone(), handle);
            }
        } else {
            let mut handle = WorkerHandle::spawn(&binary_path, chain)
                .with_context(|| format!("spawning worker for chain {}", chain.id))?;
            handle.send_start(chain, initial_pt.clone()).await?;
            handles.insert(chain.id.clone(), handle);
        }
    }

    info!("all workers spawned/reattached — waiting for Ready");

    // Wait for all workers to signal Ready.
    wait_all_ready(&mut handles).await;

    info!("all workers ready");

    // Push chain → SOCKS5 mapping to the local TUN process.
    push_tun_registry(&config).await;

    // ── Supervisor-level router SOCKS5 listener ───────────────────────────────
    // When `[routing]` is configured in supervisor.toml, bind a router-facing
    // SOCKS5 port. Connections to this port are dispatched to worker SOCKS5
    // ports using the configured policy (default: round-robin).
    if let Some(routing_cfg) = &config.routing {
        let bind_str = routing_cfg.socks5_bind.clone();
        match bind_str.parse::<std::net::SocketAddr>() {
            Err(e) => {
                warn!(bind = %bind_str, "routing.socks5_bind is invalid: {e} — router listener not started");
            }
            Ok(bind_addr) => {
                let whitelist: Option<&Vec<String>> = routing_cfg.chain_ids.as_ref();
                let router_chains: Vec<router_listener::RouterChain> = config
                    .chains
                    .iter()
                    .filter(|c| match whitelist {
                        Some(ids) => ids.iter().any(|id| id == &c.id),
                        None => true,
                    })
                    .filter_map(|c| {
                        c.socks5_bind.parse::<std::net::SocketAddr>().ok().map(|addr| {
                            router_listener::RouterChain {
                                chain_id: c.id.clone(),
                                socks5_addr: addr,
                                policy: mirage_client_core::socks5::SocksFailurePolicy::from_chain_config(
                                    c.kill_switch.as_ref(),
                                    c.group.as_deref(),
                                    &c.id,
                                ),
                                nature: c.nature,
                            }
                        })
                    })
                    .collect();

                if router_chains.is_empty() {
                    warn!("routing configured but no valid chain socks5_bind addresses — router listener not started");
                } else {
                    let router = router_listener::SupervisorRouter::new(router_chains);
                    router_listener::spawn_router_listener(bind_addr, router);
                    info!(addr = %bind_addr, "supervisor router listener spawned");
                }
            }
        }
    }

    // ── Supervisor-level DNS forwarder ────────────────────────────────────────
    // When `[dns] enabled = true` AND a router is configured, run a local DoH
    // forwarder that tunnels DNS queries through the router's SOCKS5 egress, so
    // DNS exits via the chains (no system-resolver leak). The forwarder's SOCKS5
    // upstream is pinned to the router bind regardless of `dns.socks_addr`.
    // Process-wide singleton (binds one fixed UDP port) → supervisor-level, not
    // per-worker.
    if let Some(dns_cfg) = &config.dns {
        if dns_cfg.enabled {
            match &config.routing {
                None => warn!(
                    "dns.enabled but no [routing] configured — the DNS forwarder needs the \
                     router's SOCKS5 egress; not started"
                ),
                Some(routing_cfg) => {
                    let mut dns_cfg = dns_cfg.clone();
                    dns_cfg.socks_addr = format!("socks5h://{}", routing_cfg.socks5_bind);
                    let listen = dns_cfg.listen_addr.clone();
                    match mirage_client_core::dns_forwarder::DnsForwarder::new(dns_cfg) {
                        Ok(fwd) => {
                            let fwd = std::sync::Arc::new(fwd);
                            tokio::spawn(async move {
                                if let Err(e) = fwd.run().await {
                                    error!("dns forwarder exited with error: {e}");
                                }
                            });
                            info!(
                                listen = %listen, socks = %routing_cfg.socks5_bind,
                                "supervisor DNS forwarder spawned (DoH via router SOCKS5)"
                            );
                        }
                        Err(e) => {
                            warn!("dns forwarder construction failed: {e} — not started")
                        }
                    }
                }
            }
        }
    }

    // ── Supervisor-level HTTP CONNECT proxy ───────────────────────────────────
    // Process-wide singleton (one fixed TCP port) → supervisor-level, not
    // per-worker. When enabled AND a router is configured, chain each CONNECT
    // through the router's SOCKS5 egress.
    if let Some(http_cfg) = &config.http_proxy {
        if http_cfg.enabled {
            match &config.routing {
                None => warn!(
                    "http_proxy.enabled but no [routing] configured — the HTTP CONNECT proxy \
                     needs the router's SOCKS5 egress; not started"
                ),
                Some(routing_cfg) => match http_cfg.bind_addr.parse::<std::net::SocketAddr>() {
                    Err(e) => warn!(
                        bind = %http_cfg.bind_addr,
                        "http_proxy.bind_addr invalid: {e} — not started"
                    ),
                    Ok(bind_addr) => {
                        let socks = routing_cfg.socks5_bind.clone();
                        let server =
                            mirage_client_core::http_proxy::HttpProxyServer::new_with_socks_upstream(
                                bind_addr,
                                socks.clone(),
                            );
                        tokio::spawn(async move {
                            if let Err(e) = server.run_via_socks().await {
                                error!("HTTP CONNECT proxy exited with error: {e}");
                            }
                        });
                        info!(
                            bind = %bind_addr, socks = %socks,
                            "supervisor HTTP CONNECT proxy spawned (via router SOCKS5)"
                        );
                    }
                },
            }
        }
    }

    // ── MonitorLink: delegate to the designated worker ────────────────────────
    // The supervisor no longer owns a MonitorLink. Instead it finds the chain
    // with `monitor_link_host = true` and sends `SetupMonitorLink` to that
    // worker. The worker constructs MonitorLink using its own ChainSet so the
    // OverChain transport can actually dial. Peer table snapshots flow back
    // via `WorkerMsg::PeerTableSnapshot` and are broadcast to all other workers.

    // Channel: peer_table JSON forwarded from the MonitorLink host worker to
    // the main loop for broadcast to all other workers.
    let (pt_tx, mut pt_rx) = mpsc::channel::<String>(8);

    // Build the resolved SetupMonitorLink params once and store them so the
    // respawn path (B2b) can re-send without reconstructing from config.
    let ml_setup: Option<MlSetup> = build_ml_setup(&config);

    match &ml_setup {
        None => {
            warn!("supervisor: no chain has monitor_link_host = true (or transport build failed) — running without MonitorLink (peer_table will not update)");
        }
        Some(setup) => {
            if let Some(handle) = handles.get_mut(&setup.host_chain_id) {
                if let Err(e) = handle.send_setup_monitor_link(
                    setup.monitor_urls.clone(),
                    setup.monitor_api_key.clone(),
                    setup.transports_json.clone(),
                    setup.tun_api_url.clone(),
                ).await {
                    warn!(
                        chain_id = %setup.host_chain_id,
                        "supervisor: failed to send SetupMonitorLink: {e}"
                    );
                } else {
                    info!(
                        chain_id = %setup.host_chain_id,
                        "supervisor: SetupMonitorLink sent to worker"
                    );
                }
            } else {
                warn!(
                    chain_id = %setup.host_chain_id,
                    "supervisor: monitor_link_host chain has no live handle — MonitorLink not delegated"
                );
            }
        }
    }

    // OTA poller channel.
    let (ota_tx, mut ota_rx) = mpsc::channel::<UpdateAvailable>(4);
    let current_version = mirage_chain_ota::current_version().to_string();

    spawn_ota_poller(
        config.monitor_url.clone(),
        config.ota_pubkey.clone(),
        config.ota_poll_interval_secs,
        current_version,
        ota_tx,
    );

    // ── Config-reload channel ─────────────────────────────────────────────────
    // Both the mtime poll task and the control HTTP endpoint send `()` here to
    // trigger an immediate reconcile in the main loop.
    let (reload_tx, mut reload_rx) = mpsc::channel::<()>(4);

    // Shared per-worker status snapshot: the main loop writes it each iteration,
    // the control HTTP endpoint serves it on GET /status and /admin/health.
    let shared_status: control_endpoint::SharedStatus =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    // ── Control HTTP endpoint (optional) ─────────────────────────────────────
    if let Some(ref bind_str) = config.control_http_bind {
        match bind_str.parse::<std::net::SocketAddr>() {
            Err(e) => {
                warn!(bind = %bind_str, "control_http_bind is invalid: {e} — control endpoint not started");
            }
            Ok(bind_addr) => {
                spawn_control_endpoint(bind_addr, reload_tx.clone(), std::sync::Arc::clone(&shared_status));
            }
        }
    }

    // ── Config-file mtime poll task ───────────────────────────────────────────
    // Polls every `reload_poll_secs` seconds.  Sends `()` on `reload_tx` when
    // the file's mtime advances (i.e. the config was written by the sentinel or
    // the operator).
    {
        let poll_path = config
            .reload_config_path
            .clone()
            .expect("reload_config_path is always Some after startup normalisation");
        let poll_interval = tokio::time::Duration::from_secs(config.reload_poll_secs);
        let poll_tx = reload_tx.clone();
        tokio::spawn(async move {
            // Seed with current mtime so we don't trigger on startup.
            let mut last_mtime = std::fs::metadata(&poll_path)
                .and_then(|m| m.modified())
                .ok();
            loop {
                tokio::time::sleep(poll_interval).await;
                let current_mtime = std::fs::metadata(&poll_path)
                    .and_then(|m| m.modified())
                    .ok();
                if current_mtime != last_mtime && current_mtime.is_some() {
                    debug!(path = %poll_path, "config file changed — signalling reconcile");
                    last_mtime = current_mtime;
                    // Best-effort: if the channel is full the main loop will catch
                    // the change on the next poll tick.
                    let _ = poll_tx.try_send(());
                }
            }
        });
    }

    // Shutdown signal.
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        // Poll all worker rx channels for events (non-blocking pass).
        // `PeerTableSnapshot` is also routed to `pt_tx` for broadcast;
        // all messages flow through `apply_msg` + `log_worker_event` as before.
        let chain_ids: Vec<String> = handles.keys().cloned().collect();
        for chain_id in &chain_ids {
            let Some(handle) = handles.get_mut(chain_id) else { continue };
            while let Ok(event) = handle.rx.try_recv() {
                if let WorkerMsg::PeerTableSnapshot { generation, ref peer_table_json } = event.msg {
                    debug!(
                        source_chain = %chain_id,
                        generation,
                        "supervisor: PeerTableSnapshot from MonitorLink host worker — broadcasting"
                    );
                    if let Err(e) = state::save_peer_table(&state_dir_path, peer_table_json) {
                        warn!("supervisor: failed to persist peer_table snapshot: {e}");
                    }
                    let _ = pt_tx.try_send(peer_table_json.clone());
                }
                handle.apply_msg(&event.msg);
                log_worker_event(chain_id, &event.msg);
            }
        }

        // Refresh the shared status snapshot served by the control endpoint.
        {
            let snap: Vec<control_endpoint::WorkerStatusSnapshot> = handles
                .iter()
                .map(|(id, h)| control_endpoint::WorkerStatusSnapshot {
                    chain_id: id.clone(),
                    pid: h.pid,
                    state: format!("{:?}", h.state),
                })
                .collect();
            if let Ok(mut s) = shared_status.lock() {
                *s = snap;
            }
        }

        // Detect dead workers and respawn unless the death was intentional or
        // the chain has hit the restart rate limit.
        let chain_configs: Vec<_> = config.chains.clone();
        let mut to_respawn: Vec<String> = Vec::new();

        for (chain_id, handle) in handles.iter_mut() {
            if handle.state == HandleState::Exited {
                continue;
            }
            if let Some(code) = handle.try_wait() {
                if handle.intentional_restart {
                    info!(chain_id = %chain_id, "worker exited intentionally — skip respawn");
                    handle.intentional_restart = false;
                    continue;
                }
                warn!(
                    chain_id = %chain_id,
                    code = ?code,
                    "worker died unexpectedly"
                );
                to_respawn.push(chain_id.clone());
            }
        }

        for chain_id in to_respawn {
            let Some(chain_cfg) = chain_configs.iter().find(|c| c.id == chain_id) else {
                continue;
            };

            let old_tracker = handles
                .remove(&chain_id)
                .map(|h| h.restart_tracker)
                .unwrap_or_default();

            let mut tracker = old_tracker;
            if !tracker.record() {
                error!(
                    chain_id = %chain_id,
                    total = tracker.total,
                    "worker hit restart limit — will not respawn"
                );
                continue;
            }

            info!(
                chain_id = %chain_id,
                restart_count = tracker.total,
                "respawning worker"
            );

            match WorkerHandle::spawn_with_tracker(&binary_path, chain_cfg, tracker) {
                Err(e) => {
                    error!(chain_id = %chain_id, "respawn failed: {e:#}");
                }
                Ok(mut new_handle) => {
                    // Use the last persisted peer_table on respawn so the worker
                    // doesn't start with an empty pool.
                    let respawn_pt = state::load_peer_table(&state_dir_path);
                    if let Err(e) = new_handle.send_start(chain_cfg, respawn_pt).await {
                        error!(chain_id = %chain_id, "send_start after respawn failed: {e:#}");
                    }

                    // B2b: if the respawned worker is the ML-host, re-send
                    // SetupMonitorLink so the MonitorLink comes back up without
                    // requiring a supervisor restart.
                    if let Some(ref setup) = ml_setup {
                        if setup.host_chain_id == chain_id {
                            info!(
                                chain_id = %chain_id,
                                "supervisor: ML-host worker respawned — re-sending SetupMonitorLink"
                            );
                            if let Err(e) = new_handle.send_setup_monitor_link(
                                setup.monitor_urls.clone(),
                                setup.monitor_api_key.clone(),
                                setup.transports_json.clone(),
                                setup.tun_api_url.clone(),
                            ).await {
                                warn!(
                                    chain_id = %chain_id,
                                    "supervisor: re-send SetupMonitorLink after respawn failed: {e:#}"
                                );
                            }
                        }
                    }

                    handles.insert(chain_id, new_handle);
                }
            }
        }

        tokio::select! {
            _ = &mut shutdown => {
                info!("ctrl-c — shutting down all workers");
                shutdown_all(&mut handles).await;
                break;
            }

            _ = fatal_rx.recv() => {
                warn!("memory guard: fatal threshold — shutting down workers and exiting");
                shutdown_all(&mut handles).await;
                std::process::exit(0);
            }

            pt_json = pt_rx.recv() => {
                if let Some(json) = pt_json {
                    // Broadcast PeerTableUpdate to all live workers EXCEPT the
                    // MonitorLink host (it already has the data via its own ArcSwap).
                    let host_id = config.chains.iter()
                        .find(|c| c.monitor_link_host)
                        .map(|c| c.id.as_str());
                    for (chain_id, handle) in handles.iter_mut() {
                        if handle.state == HandleState::Exited {
                            continue;
                        }
                        if Some(chain_id.as_str()) == host_id {
                            // Source worker — skip (it owns MonitorLink's ArcSwap).
                            continue;
                        }
                        if let Err(e) = handle.send_peer_table_update(json.clone()).await {
                            warn!(chain_id = %chain_id, "send PeerTableUpdate failed: {e:#}");
                        }
                    }
                }
            }

            update = ota_rx.recv() => {
                match update {
                    None => {
                        warn!("OTA poller channel closed");
                    }
                    Some(upd) => {
                        info!(version = %upd.version, "OTA update triggered — rolling rollout");
                        match download_new_binary(&upd.monitor_url, &upd.ota_pubkey).await {
                            Err(e) => {
                                error!("OTA download failed: {e:#}");
                            }
                            Ok(new_binary) => {
                                for handle in handles.values_mut() {
                                    handle.intentional_restart = true;
                                }
                                if let Err(e) = rolling_update(
                                    &mut handles,
                                    &config.chains,
                                    &new_binary,
                                    &state_dir_path,
                                )
                                .await
                                {
                                    error!("rolling update failed: {e:#}");
                                } else {
                                    push_tun_registry(&config).await;

                                    // Supervisor self-update: spawn new binary and exit.
                                    // Workers survive because they are fully detached and
                                    // keep their pipes listening. The new supervisor will
                                    // discover them via discover_orphans() on startup.
                                    let self_path = std::env::current_exe()
                                        .unwrap_or_else(|_| new_binary.clone());
                                    if new_binary == self_path
                                        || new_binary.file_name() == self_path.file_name()
                                    {
                                        info!(
                                            new_binary = ?new_binary,
                                            "supervisor binary updated — handing off to new process"
                                        );
                                        match std::process::Command::new(&new_binary)
                                            .args(std::env::args().skip(1))
                                            .spawn()
                                        {
                                            Ok(_) => {
                                                info!("new supervisor spawned — exiting");
                                                std::process::exit(0);
                                            }
                                            Err(e) => {
                                                error!("failed to spawn new supervisor: {e}");
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // ── Config-reload trigger (mtime poll or control endpoint) ─────────
            _ = reload_rx.recv() => {
                // Drain any additional signals that queued up while we were busy
                // so we do one reconcile, not N.
                while reload_rx.try_recv().is_ok() {}

                let reload_path = config
                    .reload_config_path
                    .as_deref()
                    .unwrap_or(&config_path);
                info!(path = %reload_path, "config-reload triggered — reconciling chains");

                if let Some(new_config) = try_reload_config(reload_path) {
                    // Replace in-memory config so the new chain set is used for
                    // future peer_table broadcasts, TUN registry, etc.
                    config = new_config;
                    // Normalise reload_config_path again in case it was absent.
                    if config.reload_config_path.is_none() {
                        config.reload_config_path = Some(config_path.clone());
                    }
                    reconcile(
                        &mut handles,
                        &config,
                        &binary_path,
                        &state_dir_path,
                    ).await;
                }
            }

            // Yield briefly so the try_recv / try_wait loop can drain events.
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {}
        }
    }

    info!("supervisor exiting");
    Ok(())
}

/// Wait until every worker handle has transitioned to Ready.
async fn wait_all_ready(handles: &mut HashMap<String, WorkerHandle>) {
    let timeout = tokio::time::Duration::from_secs(60);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let pending: Vec<&str> = handles
            .values()
            .filter(|h| h.state != HandleState::Ready)
            .map(|h| h.chain_id.as_str())
            .collect();

        if pending.is_empty() {
            break;
        }

        if tokio::time::Instant::now() >= deadline {
            warn!(
                still_pending = ?pending,
                "timeout waiting for workers to become Ready — proceeding anyway"
            );
            break;
        }

        for handle in handles.values_mut() {
            if let Ok(event) = handle.rx.try_recv() {
                handle.apply_msg(&event.msg);
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
}

fn log_worker_event(chain_id: &str, msg: &WorkerMsg) {
    match msg {
        WorkerMsg::Ready { pid, .. } => {
            info!(chain_id, pid, "worker Ready");
        }
        WorkerMsg::Draining {
            active_connections, ..
        } => {
            info!(chain_id, active_connections, "worker Draining");
        }
        WorkerMsg::Drained { .. } => {
            info!(chain_id, "worker Drained");
        }
        WorkerMsg::Status {
            active_connections,
            total_connections,
            state,
            ..
        } => {
            info!(
                chain_id,
                active_connections,
                total_connections,
                ?state,
                "worker Status"
            );
        }
        WorkerMsg::Error { message, .. } => {
            error!(chain_id, "worker Error: {message}");
        }
        WorkerMsg::Hello { pid, .. } => {
            info!(chain_id, pid, "worker Hello");
        }
        WorkerMsg::PeerTableSnapshot { generation, .. } => {
            // Routed to pt_tx before reaching this function; logged there.
            debug!(chain_id, generation, "worker PeerTableSnapshot");
        }
    }
}
