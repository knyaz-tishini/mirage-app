//! Periodic telemetry push to mirage2monitor.
//!
//! Pulls metrics from TUN (`GET http://127.0.0.1:19485/metrics` and
//! `/connections`) every `interval_secs` seconds and POSTs the aggregated
//! [`TelemetryPush`] to `{monitor_url}/relay/clients/{client_id}/telemetry`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

// ── TUN port ──────────────────────────────────────────────────────────────────

const TUN_BASE_URL: &str = "http://127.0.0.1:19485";

// ── Payload shapes ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TelemetryPush {
    pub ts_ms: u64,
    pub chains: HashMap<String, ChainStats>,
    pub destinations_top: Vec<DestStat>,
}

#[derive(Debug, Serialize)]
pub struct ChainStats {
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub active_streams: u32,
}

#[derive(Debug, Serialize)]
pub struct DestStat {
    pub ip: String,
    pub domain: Option<String>,
    pub port: u16,
    pub bytes: u64,
}

// ── TUN response shapes (as returned by mirage2tun) ───────────────────────────

#[derive(Debug, Deserialize)]
struct TunMetrics {
    #[serde(default)]
    chains: HashMap<String, TunChainMetrics>,
}

#[derive(Debug, Deserialize)]
struct TunChainMetrics {
    #[serde(default)]
    bytes_tx: u64,
    #[serde(default)]
    bytes_rx: u64,
    #[serde(default)]
    active_streams: u32,
}

#[derive(Debug, Deserialize)]
struct TunConnection {
    ip: String,
    #[serde(default)]
    domain: Option<String>,
    port: u16,
    #[serde(default)]
    bytes_tx: u64,
    #[serde(default)]
    bytes_rx: u64,
}

// ── Spawn ─────────────────────────────────────────────────────────────────────

/// Spawn the telemetry push task.
///
/// Exits quietly if `interval_secs == 0` or credentials are missing.
pub fn spawn(
    monitor_url: String,
    monitor_api_key: String,
    client_id: String,
    interval_secs: u64,
    tun_alive_rx: watch::Receiver<bool>,
) {
    if interval_secs == 0 {
        return;
    }
    tokio::spawn(telemetry_task(
        monitor_url,
        monitor_api_key,
        client_id,
        interval_secs,
        tun_alive_rx,
    ));
}

async fn telemetry_task(
    monitor_url: String,
    api_key: String,
    client_id: String,
    interval_secs: u64,
    tun_alive_rx: watch::Receiver<bool>,
) {
    let http = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("[telemetry] failed to build http client: {}", e);
            return;
        }
    };

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        // Skip if tun is not alive
        if !*tun_alive_rx.borrow() {
            continue;
        }

        let push = match collect_telemetry(&http).await {
            Some(p) => p,
            None => continue,
        };

        post_telemetry(&http, &monitor_url, &api_key, &client_id, &push).await;
    }
}

async fn collect_telemetry(http: &reqwest::Client) -> Option<TelemetryPush> {
    let metrics_url = format!("{}/metrics", TUN_BASE_URL);
    let conns_url = format!("{}/connections", TUN_BASE_URL);

    let metrics_resp = http.get(&metrics_url).send().await.ok()?;
    if !metrics_resp.status().is_success() {
        return None;
    }
    let tun_metrics: TunMetrics = metrics_resp.json().await.ok()?;

    let chains = tun_metrics
        .chains
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                ChainStats {
                    bytes_tx: v.bytes_tx,
                    bytes_rx: v.bytes_rx,
                    active_streams: v.active_streams,
                },
            )
        })
        .collect();

    // connections are optional — if the endpoint fails, still push with empty top
    let destinations_top = match http.get(&conns_url).send().await {
        Ok(r) if r.status().is_success() => {
            match r.json::<Vec<TunConnection>>().await {
                Ok(mut conns) => {
                    // Sort descending by total bytes
                    conns.sort_unstable_by(|a, b| {
                        let a_total = a.bytes_tx.saturating_add(a.bytes_rx);
                        let b_total = b.bytes_tx.saturating_add(b.bytes_rx);
                        b_total.cmp(&a_total)
                    });
                    conns
                        .into_iter()
                        .take(8)
                        .map(|c| DestStat {
                            ip: c.ip,
                            domain: c.domain,
                            port: c.port,
                            bytes: c.bytes_tx.saturating_add(c.bytes_rx),
                        })
                        .collect()
                }
                Err(_) => Vec::new(),
            }
        }
        _ => Vec::new(),
    };

    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    Some(TelemetryPush { ts_ms, chains, destinations_top })
}

async fn post_telemetry(
    http: &reqwest::Client,
    monitor_url: &str,
    api_key: &str,
    client_id: &str,
    push: &TelemetryPush,
) {
    let url = format!(
        "{}/relay/clients/{}/telemetry",
        monitor_url.trim_end_matches('/'),
        client_id
    );
    let body = match serde_json::to_string(push) {
        Ok(b) => b,
        Err(e) => {
            warn!("[telemetry] serialise error: {}", e);
            return;
        }
    };
    match http
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_key))
        .body(body)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            info!("[telemetry] pushed to monitor (chains={})", push.chains.len());
        }
        Ok(r) => {
            warn!("[telemetry] monitor returned {}", r.status());
        }
        Err(e) => {
            warn!("[telemetry] POST failed: {}", e);
        }
    }
}
