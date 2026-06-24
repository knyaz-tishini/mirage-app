//! Periodic telemetry push to mirage2monitor.
//!
//! Pulls metrics from TUN (`GET http://127.0.0.1:19485/metrics` and
//! `/connections`) every `interval_secs` seconds and POSTs the aggregated
//! [`TelemetryPush`] to `/relay/clients/{client_id}/telemetry` over the
//! native Mirage wire (V9 encrypted tunnel — no plain HTTPS).
//!
//! When the monitor pushes a [`MetadataPolicy`] with `collect == true` the
//! same tick also aggregates a [`ClientMetadataReport`] from `/connections`
//! and POSTs it to `/relay/clients/{client_id}/metadata` over the same wire.
//!
//! Skips ticks when wire coords are not yet available (early boot).
//! Nothing is persisted — all aggregation is in-memory only.

use std::collections::HashMap;

use mirage_core::metadata::{ClientMetadataReport, FlowObservation, MetadataPolicy};
use mirage_core::split_policy::SplitPolicyAction;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::monitor_wire::WireCoords;

// ── TUN port ──────────────────────────────────────────────────────────────────

const TUN_BASE_URL: &str = "http://127.0.0.1:19485";

/// Default maximum flow entries per metadata report when the policy sends 0.
const DEFAULT_MAX_ENTRIES: u32 = 200;

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

/// Mirrors `ConnSnapshot` from `mirage-tun-windows/src/metrics.rs`.
/// The exact field names must match the JSON served by `GET /connections`.
#[derive(Debug, Deserialize)]
struct TunConnection {
    /// Chain name (e.g. `"default"`, `"__bypass__"`).
    #[serde(default)]
    chain: String,
    dest_ip: String,
    dest_port: u16,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    bytes_tx: u64,
    #[serde(default)]
    bytes_rx: u64,
    /// Full normalized exe path (Windows only, `null` when attribution missed).
    #[serde(default)]
    exe_path: Option<String>,
}

// ── Spawn ─────────────────────────────────────────────────────────────────────

/// Spawn the telemetry push task.
///
/// Exits quietly if `interval_secs == 0` (disabled by default).
/// Skips each tick when `wire_coords_rx` carries `None` (wire not yet known).
///
/// `config_rx` — watch receiver for the latest `MonitorConfig` snapshot.
/// When the config carries a `metadata_policy` with `collect == true` the
/// task also uploads a flow-metadata report over the same wire on every tick.
pub fn spawn(
    wire_coords_rx: watch::Receiver<Option<WireCoords>>,
    monitor_api_key: String,
    client_id: String,
    interval_secs: u64,
    tun_alive_rx: watch::Receiver<bool>,
    config_rx: watch::Receiver<Option<mirage_client_core::monitor_link::MonitorConfig>>,
) {
    if interval_secs == 0 {
        return;
    }
    tokio::spawn(telemetry_task(
        wire_coords_rx,
        monitor_api_key,
        client_id,
        interval_secs,
        tun_alive_rx,
        config_rx,
    ));
}

async fn telemetry_task(
    wire_coords_rx: watch::Receiver<Option<WireCoords>>,
    api_key: String,
    client_id: String,
    interval_secs: u64,
    tun_alive_rx: watch::Receiver<bool>,
    config_rx: watch::Receiver<Option<mirage_client_core::monitor_link::MonitorConfig>>,
) {
    // Local reqwest client — used only to query the TUN sidecar on localhost.
    // No outbound monitor traffic uses this client.
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

        // Skip if wire coords not yet known
        let coords = wire_coords_rx.borrow().clone();
        let Some(coords) = coords else {
            continue;
        };

        // Read the latest metadata policy from the monitor config snapshot.
        let metadata_policy: Option<MetadataPolicy> = config_rx
            .borrow()
            .as_ref()
            .and_then(|c| c.metadata_policy.clone());

        // Fetch raw connections from TUN once, reuse for both telemetry and metadata.
        let connections = fetch_connections(&http).await;

        let push = build_telemetry(&http, &connections).await;
        if let Some(p) = push {
            post_telemetry_wire(&coords, &api_key, &client_id, &p).await;
        }

        // Metadata collection — only when the policy says to collect.
        if let Some(policy) = metadata_policy {
            if policy.collect {
                let report = build_metadata_report(&connections, &policy, interval_secs);
                post_metadata_wire(&coords, &api_key, &client_id, &report).await;
            }
        }
    }
}

// ── TUN fetch helpers ─────────────────────────────────────────────────────────

async fn fetch_connections(http: &reqwest::Client) -> Vec<TunConnection> {
    let url = format!("{}/connections", TUN_BASE_URL);
    match http.get(&url).send().await {
        Ok(r) if r.status().is_success() => r.json::<Vec<TunConnection>>().await.unwrap_or_default(),
        _ => Vec::new(),
    }
}

async fn build_telemetry(
    http: &reqwest::Client,
    connections: &[TunConnection],
) -> Option<TelemetryPush> {
    let metrics_url = format!("{}/metrics", TUN_BASE_URL);

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

    // Sort descending by total bytes; take top 8.
    let mut sorted_conns: Vec<&TunConnection> = connections.iter().collect();
    sorted_conns.sort_unstable_by(|a, b| {
        let a_total = a.bytes_tx.saturating_add(a.bytes_rx);
        let b_total = b.bytes_tx.saturating_add(b.bytes_rx);
        b_total.cmp(&a_total)
    });
    let destinations_top = sorted_conns
        .into_iter()
        .take(8)
        .map(|c| DestStat {
            ip: c.dest_ip.clone(),
            domain: c.domain.clone(),
            port: c.dest_port,
            bytes: c.bytes_tx.saturating_add(c.bytes_rx),
        })
        .collect();

    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    Some(TelemetryPush { ts_ms, chains, destinations_top })
}

// ── Metadata aggregation ──────────────────────────────────────────────────────

/// Aggregate a [`ClientMetadataReport`] from raw TUN connection snapshots.
///
/// Deduplication key: `(dest_ip, dest_port, domain, app, decision)`.
/// Connections are grouped by this tuple and their occurrence counts summed.
fn build_metadata_report(
    connections: &[TunConnection],
    policy: &MetadataPolicy,
    tick_interval_secs: u64,
) -> ClientMetadataReport {
    let window_secs = if policy.sample_window_secs > 0 {
        policy.sample_window_secs
    } else {
        // Cast: tick_interval_secs fits comfortably in u32 for any sane interval.
        tick_interval_secs.min(u32::MAX as u64) as u32
    };

    let max_entries = if policy.max_entries_per_report > 0 {
        policy.max_entries_per_report as usize
    } else {
        DEFAULT_MAX_ENTRIES as usize
    };

    // Aggregate by dedup key: (dest_ip, dest_port, domain, app, decision).
    // `HashMap` key is a tuple of owned strings to avoid lifetime complexity.
    type DedupeKey = (String, u16, Option<String>, Option<String>, bool);
    let mut agg: HashMap<DedupeKey, u32> = HashMap::new();

    for conn in connections {
        // Apply domain blocklist.
        if let Some(ref dom) = conn.domain {
            if policy.domain_blocklist.iter().any(|b| b == dom) {
                continue;
            }
        }

        let decision_is_bypass = conn.chain == "__bypass__";

        let app = if policy.include_app {
            conn.exe_path.as_deref().map(|p| {
                // Use just the basename for brevity (and privacy).
                std::path::Path::new(p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(p)
                    .to_owned()
            })
        } else {
            None
        };

        let key: DedupeKey = (
            conn.dest_ip.clone(),
            conn.dest_port,
            conn.domain.clone(),
            app,
            decision_is_bypass,
        );
        *agg.entry(key).or_insert(0) += 1;
    }

    // Convert aggregation map to sorted Vec<FlowObservation>, capped at max_entries.
    let mut flows: Vec<FlowObservation> = agg
        .into_iter()
        .map(|((dest_ip, dest_port, domain, app, is_bypass), count)| FlowObservation {
            dest_ip,
            dest_port: Some(dest_port),
            domain,
            app,
            decision: if is_bypass {
                SplitPolicyAction::Bypass
            } else {
                SplitPolicyAction::Mirage
            },
            count,
        })
        .collect();

    // Stable sort by count descending so the most-observed flows survive the cap.
    flows.sort_unstable_by(|a, b| b.count.cmp(&a.count));
    flows.truncate(max_entries);

    ClientMetadataReport { window_secs, flows }
}

// ── Wire POST helpers ─────────────────────────────────────────────────────────

async fn post_telemetry_wire(
    coords: &WireCoords,
    api_key: &str,
    client_id: &str,
    push: &TelemetryPush,
) {
    let path = format!("/relay/clients/{}/telemetry", client_id);
    let body = match serde_json::to_vec(push) {
        Ok(b) => b,
        Err(e) => {
            warn!("[telemetry] serialise error: {}", e);
            return;
        }
    };
    match mirage_client_core::monitor_link::wire_post(
        &coords.monitor_addr,
        &coords.monitor_pubkey_hex,
        &coords.mesh_id,
        &path,
        api_key,
        &body,
    )
    .await
    {
        Ok((status, _)) if (200..300).contains(&(status as usize)) => {
            info!("[telemetry] pushed to monitor via wire (chains={})", push.chains.len());
        }
        Ok((status, _)) => {
            warn!("[telemetry] monitor returned {} via wire", status);
        }
        Err(e) => {
            warn!("[telemetry] wire POST failed: {}", e);
        }
    }
}

async fn post_metadata_wire(
    coords: &WireCoords,
    api_key: &str,
    client_id: &str,
    report: &ClientMetadataReport,
) {
    let path = format!("/relay/clients/{}/metadata", client_id);
    let body = match serde_json::to_vec(report) {
        Ok(b) => b,
        Err(e) => {
            warn!("[telemetry] metadata serialise error: {}", e);
            return;
        }
    };
    match mirage_client_core::monitor_link::wire_post(
        &coords.monitor_addr,
        &coords.monitor_pubkey_hex,
        &coords.mesh_id,
        &path,
        api_key,
        &body,
    )
    .await
    {
        Ok((status, _)) if (200..300).contains(&(status as usize)) => {
            info!(
                "[telemetry] metadata report uploaded via wire (flows={})",
                report.flows.len()
            );
        }
        Ok((status, _)) => {
            warn!("[telemetry] metadata upload: monitor returned {} via wire", status);
        }
        Err(e) => {
            warn!("[telemetry] metadata wire POST failed: {}", e);
        }
    }
}
