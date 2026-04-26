//! Rolling rollout controller.
//!
//! Replaces one worker at a time with a new binary:
//! spawn new → wait Ready → drain old → wait Drained/timeout → kill old.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use mirage_chain_ipc::WorkerMsg;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::{
    config::ChainConfig,
    worker_handle::{HandleState, WorkerHandle},
};

/// Default seconds supervisor waits for `Drained` before force-killing old worker.
const DRAIN_TIMEOUT_SECS: u64 = 30;

/// Drain/ready handshake timeout.
const READY_TIMEOUT_SECS: u64 = 60;

/// Perform a rolling update of all chains using `new_binary`.
///
/// `handles` is the live map of chain_id → WorkerHandle.
/// `chains` is the ordered list of chain configs (determines rollout order).
pub async fn rolling_update(
    handles: &mut HashMap<String, WorkerHandle>,
    chains: &[ChainConfig],
    new_binary: &Path,
) -> anyhow::Result<()> {
    info!(
        binary = ?new_binary,
        chain_count = chains.len(),
        "starting rolling update"
    );

    for chain in chains {
        if let Err(e) = update_one_chain(handles, chain, new_binary).await {
            error!(chain_id = %chain.id, "update failed: {e:#}");
            // Continue with remaining chains rather than aborting the whole rollout.
        }
    }

    info!("rolling update complete");
    Ok(())
}

/// Replace the worker for a single chain.
async fn update_one_chain(
    handles: &mut HashMap<String, WorkerHandle>,
    chain: &ChainConfig,
    new_binary: &Path,
) -> anyhow::Result<()> {
    info!(chain_id = %chain.id, "updating chain");

    // Spawn new worker.
    let mut new_handle = WorkerHandle::spawn(new_binary, chain)
        .with_context(|| format!("spawning new worker for {}", chain.id))?;
    new_handle.send_start(chain).await?;

    // Wait for new worker to signal Ready.
    wait_for_ready(&mut new_handle).await.with_context(|| {
        format!("waiting for new worker Ready for chain {}", chain.id)
    })?;
    info!(chain_id = %chain.id, "new worker ready");

    // Drain old worker.
    if let Some(old_handle) = handles.get_mut(&chain.id) {
        old_handle
            .send_drain(DRAIN_TIMEOUT_SECS)
            .await
            .with_context(|| format!("sending Drain to old worker for {}", chain.id))?;

        wait_for_drained(old_handle).await;
        old_handle.kill().await;
        info!(chain_id = %chain.id, "old worker drained and killed");
    } else {
        warn!(chain_id = %chain.id, "no existing handle — nothing to drain");
    }

    // Replace handle.
    handles.insert(chain.id.clone(), new_handle);
    Ok(())
}

/// Wait until the handle's state transitions to `Ready` or times out.
async fn wait_for_ready(handle: &mut WorkerHandle) -> anyhow::Result<()> {
    let deadline = Duration::from_secs(READY_TIMEOUT_SECS);
    timeout(deadline, async {
        loop {
            match handle.rx.recv().await {
                None => {
                    return Err(anyhow::anyhow!("worker exited before Ready"));
                }
                Some(event) => {
                    handle.apply_msg(&event.msg);
                    if handle.state == HandleState::Ready {
                        return Ok(());
                    }
                    if let WorkerMsg::Error { message, .. } = event.msg {
                        return Err(anyhow::anyhow!("worker error: {message}"));
                    }
                }
            }
        }
    })
    .await
    .with_context(|| format!("ready timeout ({READY_TIMEOUT_SECS}s)"))?
}

/// Wait until the handle's state transitions to `Exited` (Drained) or drain timeout expires.
async fn wait_for_drained(handle: &mut WorkerHandle) {
    let deadline = Duration::from_secs(DRAIN_TIMEOUT_SECS + 5);
    let _ = timeout(deadline, async {
        loop {
            match handle.rx.recv().await {
                None => break,
                Some(event) => {
                    handle.apply_msg(&event.msg);
                    if handle.state == HandleState::Exited {
                        break;
                    }
                }
            }
        }
    })
    .await;
}

/// Shut down all workers gracefully. Called on supervisor SIGTERM/ctrl-c.
pub async fn shutdown_all(handles: &mut HashMap<String, WorkerHandle>) {
    for (chain_id, handle) in handles.iter_mut() {
        info!(chain_id = %chain_id, "sending Shutdown");
        if let Err(e) = handle.send_shutdown().await {
            warn!(chain_id = %chain_id, "send_shutdown error: {e}");
        }
    }

    for (chain_id, handle) in handles.iter_mut() {
        let deadline = Duration::from_secs(DRAIN_TIMEOUT_SECS);
        let _ = timeout(deadline, handle.wait()).await;
        if handle.state != HandleState::Exited {
            warn!(chain_id = %chain_id, "worker did not exit — killing");
            handle.kill().await;
        }
    }
}

/// Download new binary from OTA and return path.
pub async fn download_new_binary(
    monitor_url: &str,
    ota_pubkey: &str,
) -> anyhow::Result<PathBuf> {
    use mirage_chain_ota::{download_and_verify, fetch_manifest, platform_key};

    let manifest = fetch_manifest(monitor_url).await?;
    let key = platform_key();
    let platform = manifest
        .platforms
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("no artifact for platform '{key}' in manifest"))?;

    download_and_verify(platform, ota_pubkey).await
}
