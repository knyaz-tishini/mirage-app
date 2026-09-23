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
    state,
    worker_handle::{HandleState, StopOutcome, WorkerHandle},
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
    state_dir: &Path,
    box_mode: bool,
) -> anyhow::Result<()> {
    info!(
        binary = ?new_binary,
        chain_count = chains.len(),
        "starting rolling update"
    );

    for chain in chains {
        if let Err(e) = update_one_chain(handles, chain, new_binary, state_dir, box_mode).await {
            error!(chain_id = %chain.id, "update failed: {e:#}");
            // Continue with remaining chains rather than aborting the whole rollout.
        }
    }

    info!("rolling update complete");
    Ok(())
}

/// Replace the worker for a single chain.
///
/// Box mode: the old worker is stopped only over its graceful IPC `Drain`
/// path — see `WorkerHandle::stop`. A worker that does not exit within the
/// drain wait is fatal for the whole supervisor process, mid-rollout or not:
/// there is no safe way to reclaim its `socks5_bind` port otherwise.
async fn update_one_chain(
    handles: &mut HashMap<String, WorkerHandle>,
    chain: &ChainConfig,
    new_binary: &Path,
    state_dir: &Path,
    box_mode: bool,
) -> anyhow::Result<()> {
    info!(chain_id = %chain.id, "updating chain");

    // Spawn new worker with last-known peer_table so pool bootstraps immediately.
    let rollout_pt = state::load_peer_table(state_dir);
    let mut new_handle = WorkerHandle::spawn(new_binary, chain)
        .with_context(|| format!("spawning new worker for {}", chain.id))?;
    new_handle.send_start(chain, rollout_pt, box_mode).await?;

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
        match old_handle.stop(box_mode).await {
            StopOutcome::Stopped => info!(chain_id = %chain.id, "old worker drained and stopped"),
            StopOutcome::StuckInBoxMode => {
                error!(
                    chain_id = %chain.id,
                    "box mode: old worker did not exit after Drain during OTA rollout — \
                     supervisor exiting non-zero so the warden restarts the unit"
                );
                std::process::exit(1);
            }
        }
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

/// Shut down all workers gracefully. Called on supervisor SIGTERM/ctrl-c and
/// on the memory guard's fatal threshold.
///
/// Box mode: never calls `kill()`. A worker still alive after `Shutdown` is
/// fatal for the whole supervisor process — see `WorkerHandle::stop`'s doc.
/// This exits the supervisor itself as soon as the first stuck worker is
/// found, which is correct even mid-shutdown: the caller's own subsequent
/// `std::process::exit(0)` (the ctrl-c and memory-guard call sites) would
/// otherwise mask a worker the warden needs to know did not stop cleanly.
pub async fn shutdown_all(handles: &mut HashMap<String, WorkerHandle>, box_mode: bool) {
    for (chain_id, handle) in handles.iter_mut() {
        info!(chain_id = %chain_id, "sending Shutdown");
        if let Err(e) = handle.send_shutdown().await {
            warn!(chain_id = %chain_id, "send_shutdown error: {e}");
        }
    }

    for (chain_id, handle) in handles.iter_mut() {
        let deadline = Duration::from_secs(DRAIN_TIMEOUT_SECS);
        let _ = timeout(deadline, handle.wait()).await;
        match handle.stop(box_mode).await {
            StopOutcome::Stopped => {}
            StopOutcome::StuckInBoxMode => {
                error!(
                    chain_id = %chain_id,
                    "box mode: worker did not exit after Shutdown — supervisor exiting \
                     non-zero so the warden restarts the unit"
                );
                std::process::exit(1);
            }
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
