//! Crash report struct + persistence/relay logic.

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;
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
pub async fn report_crash(
    report: &CrashReport,
    monitor_url: Option<&str>,
) {
    if let Some(url) = monitor_url {
        if try_post_crash(report, url).await.is_ok() {
            return;
        }
    }
    save_crash_to_disk(report);
}

async fn try_post_crash(report: &CrashReport, monitor_url: &str) -> Result<()> {
    // TODO: wire real client_id from onboarding (tracked in a later task)
    let url = format!("{}/relay/clients/unknown/crash", monitor_url.trim_end_matches('/'));
    let body = serde_json::to_string(report)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
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
