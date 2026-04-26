//! mirage-chain-supervisor — orchestrator process.
//!
//! Reads config, spawns one worker per chain, handles IPC, polls for OTA updates,
//! and performs rolling binary rollouts.

mod config;
mod ota_poller;
mod rollout;
mod worker_handle;

use std::collections::HashMap;

use anyhow::Context;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use config::Config;
use mirage_chain_ipc::WorkerMsg;
use ota_poller::{UpdateAvailable, spawn_ota_poller};
use rollout::{download_new_binary, rolling_update, shutdown_all};
use worker_handle::WorkerHandle;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("MIRAGE_LOG").add_directive(
                "mirage_chain_supervisor=debug".parse().expect("valid directive"),
            ),
        )
        .init();

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

    let binary_path = std::path::PathBuf::from(&config.worker_binary);

    // Spawn all workers.
    let mut handles: HashMap<String, WorkerHandle> = HashMap::new();
    for chain in &config.chains {
        let mut handle = WorkerHandle::spawn(&binary_path, chain)
            .with_context(|| format!("spawning worker for chain {}", chain.id))?;
        handle.send_start(chain).await?;
        handles.insert(chain.id.clone(), handle);
    }

    info!("all workers spawned — waiting for Ready");

    // Wait for all workers to signal Ready.
    wait_all_ready(&mut handles).await;

    info!("all workers ready");

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

    // Build merged IPC event channel.
    // Each WorkerHandle has its own rx; we poll them in the main loop.

    // Shutdown signal.
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        // Poll all worker rx channels for events (non-blocking pass).
        for (chain_id, handle) in handles.iter_mut() {
            while let Ok(event) = handle.rx.try_recv() {
                handle.apply_msg(&event.msg);
                log_worker_event(chain_id, &event.msg);
            }
        }

        tokio::select! {
            _ = &mut shutdown => {
                info!("ctrl-c — shutting down all workers");
                shutdown_all(&mut handles).await;
                break;
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
                                if let Err(e) = rolling_update(
                                    &mut handles,
                                    &config.chains,
                                    &new_binary,
                                )
                                .await
                                {
                                    error!("rolling update failed: {e:#}");
                                }
                            }
                        }
                    }
                }
            }

            // Yield briefly so the try_recv loop can drain events.
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {}
        }
    }

    info!("supervisor exiting");
    Ok(())
}

/// Wait until every worker handle has transitioned to Ready.
async fn wait_all_ready(handles: &mut HashMap<String, WorkerHandle>) {
    use worker_handle::HandleState;

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

        // Drain events.
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
    }
}
