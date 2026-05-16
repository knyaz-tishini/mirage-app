//! mirage-chain-supervisor — orchestrator process.
//!
//! Reads config, discovers live workers (pipe reattach), spawns missing ones,
//! handles IPC, polls for OTA updates, and performs rolling binary rollouts.

mod config;
mod ota_poller;
mod reattach;
mod rollout;
mod router_listener;
mod tun_registry;
mod worker_handle;

use std::collections::HashMap;

use anyhow::Context;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use config::Config;
use mirage_chain_ipc::WorkerMsg;
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use ota_poller::{UpdateAvailable, spawn_ota_poller};
use reattach::discover_orphans;
use rollout::{download_new_binary, rolling_update, shutdown_all};
use tun_registry::push_tun_registry;
use worker_handle::{HandleState, WorkerHandle};

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

    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {config_path}"))?;

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
                        if let Err(se) = new_handle.send_start(chain).await {
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
            handle.send_start(chain).await?;
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

    // ── MonitorLink: delegate to the designated worker ────────────────────────
    // The supervisor no longer owns a MonitorLink. Instead it finds the chain
    // with `monitor_link_host = true` and sends `SetupMonitorLink` to that
    // worker. The worker constructs MonitorLink using its own ChainSet so the
    // OverChain transport can actually dial. Peer table snapshots flow back
    // via `WorkerMsg::PeerTableSnapshot` and are broadcast to all other workers.

    // Channel: peer_table JSON forwarded from the MonitorLink host worker to
    // the main loop for broadcast to all other workers.
    let (pt_tx, mut pt_rx) = mpsc::channel::<String>(8);

    // Find the designated MonitorLink host chain (if any).
    let ml_host_chain = config.chains.iter().find(|c| c.monitor_link_host);

    match ml_host_chain {
        None => {
            warn!("supervisor: no chain has monitor_link_host = true — running without MonitorLink (peer_table will not update)");
        }
        Some(host_chain) => {
            let host_chain_id = host_chain.id.clone();
            if let Some(handle) = handles.get_mut(&host_chain_id) {
                // Serialize the transports list so the worker can deserialize it
                // using `mirage_chain_ipc::MonitorTransportSpec`.
                // `MonitorTransportConfig` and `MonitorTransportSpec` share the
                // same serde shape (same `kind` tag, same fields).
                match serde_json::to_string(&config.monitor_transports) {
                    Err(e) => {
                        warn!("supervisor: failed to serialize monitor_transports: {e} — MonitorLink not delegated");
                    }
                    Ok(transports_json) => {
                        if let Err(e) = handle.send_setup_monitor_link(
                            vec![config.monitor_url.clone()],
                            config.monitor_api_key.clone(),
                            transports_json,
                        ).await {
                            warn!("supervisor: failed to send SetupMonitorLink to {host_chain_id}: {e}");
                        } else {
                            info!("supervisor: SetupMonitorLink sent to worker {host_chain_id}");
                        }
                    }
                }
            } else {
                warn!(
                    chain_id = %host_chain.id,
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
                    let _ = pt_tx.try_send(peer_table_json.clone());
                }
                handle.apply_msg(&event.msg);
                log_worker_event(chain_id, &event.msg);
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
                    if let Err(e) = new_handle.send_start(chain_cfg).await {
                        error!(chain_id = %chain_id, "send_start after respawn failed: {e:#}");
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
