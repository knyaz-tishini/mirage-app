//! Periodic log-tail upload to mirage2monitor.
//!
//! Every `interval_secs` seconds, reads the last 32 KB of cli and tun stderr
//! ring buffers and POSTs each non-empty tail to
//! `/relay/clients/{client_id}/logs` over the native Mirage wire (V9
//! encrypted tunnel — no plain HTTPS).
//!
//! Skips ticks when wire coords are not yet available (early boot).

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{watch, Mutex};
use tracing::warn;

use crate::monitor_wire::WireCoords;
use crate::supervisor::{ChildState, RingBuffer};

// ── Upload size limit ─────────────────────────────────────────────────────────

const TAIL_BYTES: usize = 32 * 1024; // 32 KB per process

// ── Payload ───────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct LogUploadBody<'a> {
    ts_ms: u64,
    process: &'a str,
    stderr_tail: String,
    stdout_tail: &'a str,
}

// ── Public spawn ──────────────────────────────────────────────────────────────

/// Spawn the log-upload task.
///
/// Exits quietly if `interval_secs == 0` (disabled by default).
/// Skips each tick when `wire_coords_rx` carries `None` (wire not yet known).
pub fn spawn(
    wire_coords_rx: watch::Receiver<Option<WireCoords>>,
    monitor_api_key: String,
    client_id: String,
    interval_secs: u64,
    children: Arc<Mutex<ChildState>>,
) {
    if interval_secs == 0 {
        return;
    }
    tokio::spawn(log_upload_task(
        wire_coords_rx,
        monitor_api_key,
        client_id,
        interval_secs,
        children,
    ));
}

async fn log_upload_task(
    wire_coords_rx: watch::Receiver<Option<WireCoords>>,
    api_key: String,
    client_id: String,
    interval_secs: u64,
    children: Arc<Mutex<ChildState>>,
) {
    let mut ticker =
        tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        // Skip if wire coords not yet known
        let coords = wire_coords_rx.borrow().clone();
        let Some(coords) = coords else {
            continue;
        };

        // Snapshot stderr tails while holding the lock briefly.
        let (cli_tail, tun_tail) = {
            let ch = children.lock().await;
            let cli = match ch.cli.as_ref() {
                Some(h) => Some(read_tail(&h.stderr_buf).await),
                None => None,
            };
            let tun = match ch.tun.as_ref() {
                Some(h) => Some(read_tail(&h.stderr_buf).await),
                None => None,
            };
            (cli, tun)
        };

        if let Some(tail) = cli_tail {
            if !tail.is_empty() {
                post_log_wire(&coords, &api_key, &client_id, "cli", tail).await;
            }
        }

        if let Some(tail) = tun_tail {
            if !tail.is_empty() {
                post_log_wire(&coords, &api_key, &client_id, "tun", tail).await;
            }
        }
    }
}

async fn read_tail(buf: &tokio::sync::Mutex<RingBuffer>) -> String {
    buf.lock().await.tail_utf8(TAIL_BYTES)
}

async fn post_log_wire(
    coords: &WireCoords,
    api_key: &str,
    client_id: &str,
    process: &str,
    stderr_tail: String,
) {
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let body = LogUploadBody {
        ts_ms,
        process,
        stderr_tail,
        stdout_tail: "",
    };

    let body_bytes = match serde_json::to_vec(&body) {
        Ok(s) => s,
        Err(e) => {
            warn!("[log_upload] serialise error: {}", e);
            return;
        }
    };

    let path = format!("/relay/clients/{}/logs", client_id);

    match mirage_client_core::monitor_link::wire_post(
        &coords.monitor_addr,
        &coords.monitor_pubkey_hex,
        &coords.mesh_id,
        &path,
        api_key,
        &body_bytes,
    )
    .await
    {
        Ok((status, _)) if (200..300).contains(&(status as usize)) => {
            // best-effort, no noise on success
        }
        Ok((status, _)) => {
            warn!(
                "[log_upload] monitor returned {} via wire for process={}",
                status, process
            );
        }
        Err(e) => {
            warn!("[log_upload] wire POST failed for process={}: {}", process, e);
        }
    }
}
