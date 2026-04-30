//! Crash report struct + persistence/relay logic.

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;
use tokio::sync::watch;
use tracing::{info, warn};

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

/// Try to POST crash report to monitor; on failure (or no monitor) write to disk.
/// If POST succeeds (2xx), disk file is NOT written (no fallback needed).
/// If POST fails, write to disk so the queue drain can retry later.
pub async fn report_crash(
    report: &CrashReport,
    monitor_url: Option<&str>,
    monitor_api_key: Option<&str>,
    client_id: Option<&str>,
) {
    if let (Some(url), Some(key), Some(id)) = (monitor_url, monitor_api_key, client_id) {
        match try_post_crash(report, url, key, id).await {
            Ok(()) => return, // posted — no disk needed
            Err(e) => {
                warn!("[crash] POST to monitor failed ({}), saving to disk", e);
            }
        }
    }
    save_crash_to_disk(report);
}

async fn try_post_crash(
    report: &CrashReport,
    monitor_url: &str,
    api_key: &str,
    client_id: &str,
) -> Result<()> {
    let url = format!(
        "{}/relay/clients/{}/crash",
        monitor_url.trim_end_matches('/'),
        client_id
    );
    let body = serde_json::to_string(report)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_key))
        .body(body)
        .send()
        .await?;
    if resp.status().is_success() {
        info!("[crash] posted crash report for '{}' to monitor", report.process);
        Ok(())
    } else {
        anyhow::bail!("monitor returned {}", resp.status());
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
/// Each queued file is POSTed to the monitor; on success the file is deleted.
pub fn spawn_queue_drain(
    monitor_reachable_rx: watch::Receiver<bool>,
    monitor_url: String,
    monitor_api_key: String,
    client_id: String,
) {
    tokio::spawn(queue_drain_task(
        monitor_reachable_rx,
        monitor_url,
        monitor_api_key,
        client_id,
    ));
}

async fn queue_drain_task(
    mut reachable_rx: watch::Receiver<bool>,
    monitor_url: String,
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

        drain_disk_queue(&monitor_url, &api_key, &client_id).await;
    }
}

async fn drain_disk_queue(monitor_url: &str, api_key: &str, client_id: &str) {
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
        if try_post_raw_crash(&raw, monitor_url, api_key, client_id).await.is_ok() {
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

async fn try_post_raw_crash(
    body: &str,
    monitor_url: &str,
    api_key: &str,
    client_id: &str,
) -> Result<()> {
    let url = format!(
        "{}/relay/clients/{}/crash",
        monitor_url.trim_end_matches('/'),
        client_id
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_key))
        .body(body.to_owned())
        .send()
        .await?;
    if resp.status().is_success() {
        Ok(())
    } else {
        anyhow::bail!("monitor returned {}", resp.status());
    }
}
