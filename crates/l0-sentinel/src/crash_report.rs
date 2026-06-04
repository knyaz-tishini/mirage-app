//! Crash report struct + persistence/relay logic.
//!
//! Sends crash reports over the native Mirage wire (V9 encrypted tunnel).
//! Falls back to disk persistence when wire coords are not yet known or when
//! the wire dial fails.  The disk queue is drained to the wire when
//! `monitor_reachable_rx` flips to `true`.

use std::path::PathBuf;

use serde::Serialize;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::monitor_wire::WireCoords;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum CrashKind {
    ExitNonzero,
    Killed,
    Other,
}

#[derive(Serialize)]
pub struct CrashReport {
    pub process: String,
    pub kind: CrashKind,
    pub exit_code: Option<i32>,
    pub started_at_ms: u64,
    pub died_at_ms: u64,
    pub uptime_ms: u64,
    pub stderr_tail: String,
    pub stdout_tail: String,
    pub version: String,
    pub platform: String,
}

// ── Persist / relay ───────────────────────────────────────────────────────────

/// Try to POST crash report to monitor over the wire; on failure (or no wire
/// coords yet), write to disk.  If the wire POST succeeds (2xx), no disk file
/// is written.
pub async fn report_crash(
    report: &CrashReport,
    wire_coords: Option<&WireCoords>,
    client_id: Option<&str>,
) {
    if let (Some(coords), Some(id)) = (wire_coords, client_id) {
        match try_post_crash_wire(report, coords, id).await {
            Ok(()) => return, // posted over wire — no disk needed
            Err(e) => {
                warn!("[crash] wire POST to monitor failed ({}), saving to disk", e);
            }
        }
    }
    save_crash_to_disk(report);
}

async fn try_post_crash_wire(
    report: &CrashReport,
    coords: &WireCoords,
    client_id: &str,
) -> Result<(), String> {
    let path = format!("/relay/clients/{}/crash", client_id);
    let body = serde_json::to_vec(report)
        .map_err(|e| format!("serialize crash report: {e}"))?;
    // We use an empty bearer here: crash reports are identified by client_id
    // in the path; the monitor_api_key is carried by the queue drain when
    // available.  Pass empty string so the header is present but harmless for
    // monitors that don't require it on this endpoint.
    let (status, _) = mirage_client_core::monitor_link::wire_post(
        &coords.monitor_addr,
        &coords.monitor_pubkey_hex,
        &coords.mesh_id,
        &path,
        "",
        &body,
    )
    .await?;
    if (200..300).contains(&(status as usize)) {
        info!("[crash] posted crash report for '{}' to monitor via wire", report.process);
        Ok(())
    } else {
        Err(format!("monitor returned {status}"))
    }
}

fn save_crash_to_disk(report: &CrashReport) {
    let exe_dir = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
    {
        Some(d) => d,
        None => {
            warn!("[crash] cannot determine exe dir — crash report lost");
            return;
        }
    };
    let crashes_dir = exe_dir.join("crashes");
    if let Err(e) = std::fs::create_dir_all(&crashes_dir) {
        warn!("[crash] create crashes dir {:?}: {}", crashes_dir, e);
        return;
    }

    let ts = report.died_at_ms;
    let filename = format!("{}_{}.json", report.process, ts);
    let path = crashes_dir.join(&filename);

    let json = match serde_json::to_string_pretty(report) {
        Ok(j) => j,
        Err(e) => {
            warn!("[crash] serialise crash report: {}", e);
            return;
        }
    };

    // Truncate to 5 KB
    let json = if json.len() > 5 * 1024 {
        json[..5 * 1024].to_owned()
    } else {
        json
    };

    match std::fs::write(&path, json) {
        Ok(()) => info!("[crash] saved crash report to {:?}", path),
        Err(e) => warn!("[crash] write {:?}: {}", path, e),
    }
}

// ── Disk queue drain ──────────────────────────────────────────────────────────

/// Spawn a background task that drains `<exe_dir>/crashes/*.json` whenever
/// `monitor_reachable_rx` flips to `true`.
///
/// Each queued file is POSTed to the monitor over the wire; on success the
/// file is deleted.
pub fn spawn_queue_drain(
    monitor_reachable_rx: watch::Receiver<bool>,
    wire_coords_rx: watch::Receiver<Option<WireCoords>>,
    monitor_api_key: String,
    client_id: String,
) {
    tokio::spawn(queue_drain_task(
        monitor_reachable_rx,
        wire_coords_rx,
        monitor_api_key,
        client_id,
    ));
}

async fn queue_drain_task(
    mut reachable_rx: watch::Receiver<bool>,
    wire_coords_rx: watch::Receiver<Option<WireCoords>>,
    api_key: String,
    client_id: String,
) {
    loop {
        // Wait until monitor_reachable flips to true
        if reachable_rx.changed().await.is_err() {
            break; // sender dropped — sentinel shutting down
        }
        if !*reachable_rx.borrow() {
            continue;
        }

        // Only drain when we actually have wire coords
        let coords = wire_coords_rx.borrow().clone();
        let Some(coords) = coords else {
            continue;
        };

        drain_disk_queue(&coords, &api_key, &client_id).await;
    }
}

async fn drain_disk_queue(coords: &WireCoords, api_key: &str, client_id: &str) {
    let exe_dir = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
    {
        Some(d) => d,
        None => return,
    };
    let crashes_dir = exe_dir.join("crashes");
    if !crashes_dir.exists() {
        return;
    }

    let entries = match std::fs::read_dir(&crashes_dir) {
        Ok(e) => e,
        Err(err) => {
            warn!("[crash] read crashes dir: {}", err);
            return;
        }
    };

    let mut drained = 0u32;
    let mut failed = 0u32;

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension() else { continue };
        if ext != "json" {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                warn!("[crash] read queued file {:?}: {}", path, e);
                continue;
            }
        };
        if try_post_raw_crash_wire(&raw, coords, api_key, client_id).await.is_ok() {
            if let Err(e) = std::fs::remove_file(&path) {
                warn!("[crash] delete queued file {:?}: {}", path, e);
            } else {
                drained += 1;
            }
        } else {
            failed += 1;
        }
    }

    if drained > 0 || failed > 0 {
        info!("[crash] queue drain: {} sent, {} failed", drained, failed);
    }
}

async fn try_post_raw_crash_wire(
    body: &str,
    coords: &WireCoords,
    api_key: &str,
    client_id: &str,
) -> Result<(), String> {
    let path = format!("/relay/clients/{}/crash", client_id);
    let (status, _) = mirage_client_core::monitor_link::wire_post(
        &coords.monitor_addr,
        &coords.monitor_pubkey_hex,
        &coords.mesh_id,
        &path,
        api_key,
        body.as_bytes(),
    )
    .await?;
    if (200..300).contains(&(status as usize)) {
        Ok(())
    } else {
        Err(format!("monitor returned {status}"))
    }
}
