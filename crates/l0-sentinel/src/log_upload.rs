//! Periodic log-tail upload to mirage2monitor.
//!
//! Every `interval_secs` seconds, reads the last 32 KB of cli and tun stderr
//! ring buffers and POSTs each non-empty tail to
//! `{monitor_url}/relay/clients/{client_id}/logs`.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::warn;

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
/// Exits quietly if `interval_secs == 0` (disabled).
pub fn spawn(
    monitor_url: String,
    monitor_api_key: String,
    client_id: String,
    interval_secs: u64,
    children: Arc<Mutex<ChildState>>,
) {
    if interval_secs == 0 {
        return;
    }
    tokio::spawn(log_upload_task(
        monitor_url,
        monitor_api_key,
        client_id,
        interval_secs,
        children,
    ));
}

async fn log_upload_task(
    monitor_url: String,
    api_key: String,
    client_id: String,
    interval_secs: u64,
    children: Arc<Mutex<ChildState>>,
) {
    let http = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("[log_upload] failed to build http client: {}", e);
            return;
        }
    };

    let mut ticker =
        tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

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
                post_log(&http, &monitor_url, &api_key, &client_id, "cli", tail).await;
            }
        }

        if let Some(tail) = tun_tail {
            if !tail.is_empty() {
                post_log(&http, &monitor_url, &api_key, &client_id, "tun", tail).await;
            }
        }
    }
}

async fn read_tail(buf: &tokio::sync::Mutex<RingBuffer>) -> String {
    buf.lock().await.tail_utf8(TAIL_BYTES)
}

async fn post_log(
    http: &reqwest::Client,
    monitor_url: &str,
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

    let body_str = match serde_json::to_string(&body) {
        Ok(s) => s,
        Err(e) => {
            warn!("[log_upload] serialise error: {}", e);
            return;
        }
    };

    let url = format!(
        "{}/relay/clients/{}/logs",
        monitor_url.trim_end_matches('/'),
        client_id
    );

    match http
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_key))
        .body(body_str)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            // best-effort, no noise on success
        }
        Ok(r) => {
            warn!("[log_upload] monitor returned {} for process={}", r.status(), process);
        }
        Err(e) => {
            warn!("[log_upload] POST failed for process={}: {}", process, e);
        }
    }
}
