//! Live mesh topology + per-chain rate, polled from `mirage2tun /metrics`.
//!
//! Connection between launcher and the running TUN service is plain HTTP on
//! `127.0.0.1:19485` (default).  Once a second (configurable) we GET
//! `/metrics`, diff the byte counters against the previous snapshot to derive
//! `bytes_per_sec`, and merge that with the static chain topology read out of
//! `client-config.json` (which already lives next to the launcher exe).
//!
//! The result is published into a `tokio::sync::watch` so the render loop
//! can read the latest snapshot lock-free every frame.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::watch;

use crate::dns_resolver::DnsResolver;

const TUN_METRICS_URL: &str = "http://127.0.0.1:19485/metrics";
const TUN_CONNECTIONS_URL: &str = "http://127.0.0.1:19485/connections";
const POLL_INTERVAL: Duration = Duration::from_millis(400);
/// EMA smoothing factor for `bytes_per_sec` — small alpha = slow decay,
/// avoids the rate visibly "ringing" between two polls.
const BPS_EMA_ALPHA: f64 = 0.45;

// ── Public snapshot exposed to the renderer ──────────────────────────────────

#[derive(Default, Clone, Debug)]
pub struct MeshSnapshot {
    pub chains: Vec<ChainView>,
}

#[derive(Clone, Debug)]
pub struct ChainView {
    pub id: String,
    pub hops: Vec<HopView>, // entry .. exit
    pub bytes_per_sec: f64,
    pub last_active_ms: f64,
    /// Live destinations carried by this chain (top-N by recent traffic).
    pub destinations: Vec<DestView>,
}

#[derive(Clone, Debug)]
pub struct DestView {
    pub ip: String,
    pub port: u16,
    pub domain: Option<String>,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct HopView {
    pub addr: String,              // e.g. "91.218.245.247:4700"
    pub role: HopRole,
    pub display_name: Option<String>, // human-readable name from config, if present
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HopRole { Entry, Middle, Exit }

// ── client-config.json parsing — only the bits we need ───────────────────────

#[derive(Deserialize)]
struct ClientConfig {
    chains: Vec<ChainCfg>,
}

#[derive(Deserialize)]
struct ChainCfg {
    chain_id: String,
    #[serde(default)]
    l1: Option<HopCfg>,
    #[serde(default)]
    l2: Option<HopCfg>,
    #[serde(default)]
    hops: Vec<HopCfg>,
}

#[derive(Deserialize)]
struct HopCfg {
    addr: String,
    #[serde(default)]
    role: Option<String>, // optional in flat l1/l2 schema
    #[serde(default)]
    display_name: Option<String>,
}

fn load_chain_topology(config_path: &std::path::Path) -> HashMap<String, Vec<HopView>> {
    let raw = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    let cfg: ClientConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let mut out = HashMap::new();
    for c in cfg.chains {
        let mut hops = Vec::new();

        // Schema A: explicit `hops[]` with role strings.
        if !c.hops.is_empty() {
            for h in &c.hops {
                let role = match h.role.as_deref() {
                    Some("entry") => HopRole::Entry,
                    Some("middle") => HopRole::Middle,
                    Some("exit") => HopRole::Exit,
                    _ => HopRole::Middle,
                };
                hops.push(HopView {
                    addr: h.addr.clone(),
                    role,
                    display_name: h.display_name.clone(),
                });
            }
        } else {
            // Schema B: flat l1/l2 fields (legacy).
            if let Some(l1) = &c.l1 {
                hops.push(HopView {
                    addr: l1.addr.clone(),
                    role: HopRole::Entry,
                    display_name: l1.display_name.clone(),
                });
            }
            if let Some(l2) = &c.l2 {
                hops.push(HopView {
                    addr: l2.addr.clone(),
                    role: HopRole::Exit,
                    display_name: l2.display_name.clone(),
                });
            }
        }

        out.insert(c.chain_id, hops);
    }
    out
}

// ── /metrics JSON shape (matches mirage2tun MetricsSnapshot) ─────────────────

#[derive(Deserialize)]
struct TunMetrics {
    chains: HashMap<String, TunChainMetric>,
}

#[derive(Deserialize, Clone)]
struct TunChainMetric {
    bytes_tx: u64,
    bytes_rx: u64,
}

#[derive(Deserialize, Clone)]
struct TunConnSnapshot {
    chain: String,
    dest_ip: String,
    dest_port: u16,
    domain: Option<String>,
    bytes_tx: u64,
    bytes_rx: u64,
}

// ── Poller ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
struct ChainCounters {
    total_bytes: u64,
    last_change_at: Option<Instant>,
    /// Smoothed bytes/sec — exponentially-decaying average so the rate
    /// reading doesn't whip between values when traffic is bursty.
    smoothed_bps: f64,
}

/// Synthetic mesh snapshot for the offline mock sandbox. `tick` advances each
/// poll so `bytes_per_sec` pulses and the wheel looks alive.
fn mock_snapshot(tick: u64) -> MeshSnapshot {
    let osc = |phase: f64, base: f64| -> f64 {
        base * (1.0 + 0.5 * ((tick as f64) * 0.3 + phase).sin())
    };
    let chain = |id: &str, hops: Vec<HopView>, bps: f64, dests: Vec<DestView>| ChainView {
        id: id.to_string(),
        hops,
        bytes_per_sec: bps,
        last_active_ms: 0.0,
        destinations: dests,
    };
    let hop = |addr: &str, role: HopRole, name: &str| HopView {
        addr: addr.to_string(),
        role,
        display_name: Some(name.to_string()),
    };
    let dest = |ip: &str, port: u16, domain: &str, bytes: u64| DestView {
        ip: ip.to_string(),
        port,
        domain: Some(domain.to_string()),
        bytes,
    };
    MeshSnapshot {
        chains: vec![
            chain(
                "auto-ru-direct",
                vec![
                    hop("91.218.245.247:4700", HopRole::Entry, "l1-ru-1"),
                    hop("82.26.94.247:6700", HopRole::Exit, "tr-exit-1"),
                ],
                osc(0.0, 180_000.0),
                vec![
                    dest("140.82.121.4", 443, "github.com", 820_000),
                    dest("104.16.132.229", 443, "cloudflare.com", 410_000),
                ],
            ),
            chain(
                "auto-eu-onion",
                vec![
                    hop("151.243.173.104:4700", HopRole::Entry, "l1-nl-1"),
                    hop("45.59.119.139:5700", HopRole::Middle, "ch-relay-1"),
                    hop("82.26.94.247:6701", HopRole::Exit, "tr-exit-1"),
                ],
                osc(2.1, 90_000.0),
                vec![dest("142.250.74.78", 443, "google.com", 350_000)],
            ),
            chain(
                "auto-ru-quic",
                vec![
                    hop("193.187.94.74:4700", HopRole::Entry, "l1-ru-2"),
                    hop("45.59.119.139:6700", HopRole::Exit, "ch-exit-1"),
                ],
                osc(4.2, 40_000.0),
                vec![dest("1.1.1.1", 443, "one.one.one.one", 60_000)],
            ),
        ],
    }
}

pub fn spawn_poller(
    rt: &tokio::runtime::Handle,
    config_path: PathBuf,
    dns: Arc<DnsResolver>,
) -> watch::Receiver<MeshSnapshot> {
    let topology = load_chain_topology(&config_path);
    let (tx, rx) = watch::channel(MeshSnapshot::default());

    // Offline mock sandbox: serve an animated synthetic mesh instead of polling
    // the (absent) TUN, so the On-state wheel renders live traffic on fake data.
    if crate::model::mock_enabled() {
        let _ = dns; // unused in mock — no reverse-DNS needed
        rt.spawn(async move {
            let mut tick: u64 = 0;
            loop {
                let _ = tx.send(mock_snapshot(tick));
                tick = tick.wrapping_add(1);
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        });
        return rx;
    }

    rt.spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("reqwest client always builds with default config");

        // Per-chain rolling counters — used to derive bytes_per_sec via diff.
        let mut prev: HashMap<String, ChainCounters> = HashMap::new();
        let mut last_poll = Instant::now();

        loop {
            tokio::time::sleep(POLL_INTERVAL).await;

            // Poll metrics + connections in parallel — both are local, cheap.
            let (metrics_res, conns_res) = tokio::join!(
                client.get(TUN_METRICS_URL).send(),
                client.get(TUN_CONNECTIONS_URL).send(),
            );
            let metrics: Option<TunMetrics> = match metrics_res {
                Ok(r) if r.status().is_success() => r.json().await.ok(),
                _ => None,
            };
            let conns: Vec<TunConnSnapshot> = match conns_res {
                Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
                _ => Vec::new(),
            };
            let now = Instant::now();
            let dt = now.duration_since(last_poll).as_secs_f64().max(0.001);
            last_poll = now;

            // Group connections by chain so we can attach the top-N destinations
            // per chain in the snapshot.
            let mut conns_by_chain: HashMap<String, Vec<DestView>> = HashMap::new();
            for c in conns {
                let total = c.bytes_tx + c.bytes_rx;
                // Try to resolve the IP via reverse DNS.
                let domain = if c.domain.is_some() {
                    c.domain
                } else {
                    // Parse the IP — strip port if present.
                    let ip_str = c.dest_ip.split(':').next().unwrap_or(&c.dest_ip);
                    if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
                        dns.lookup_or_enqueue(ip)
                    } else {
                        None
                    }
                };
                conns_by_chain
                    .entry(c.chain.clone())
                    .or_default()
                    .push(DestView {
                        ip: c.dest_ip,
                        port: c.dest_port,
                        domain,
                        bytes: total,
                    });
            }
            for v in conns_by_chain.values_mut() {
                v.sort_by(|a, b| b.bytes.cmp(&a.bytes));
                v.truncate(8); // top 8 destinations per chain — keeps the UI uncluttered
            }

            // Build the per-chain view from the configured topology so idle
            // chains still appear (with empty destinations + dim styling) and
            // the user always sees the full mesh shape.
            let mut chains_view = Vec::<ChainView>::new();
            // Iterate the topology (so order is config-driven and stable);
            // overlay live counters from /metrics where present.
            let mut seen = std::collections::HashSet::new();
            for (id, hops) in &topology {
                seen.insert(id.clone());
                let cur_total = if let Some(m) = metrics.as_ref().and_then(|x| x.chains.get(id)) {
                    m.bytes_tx + m.bytes_rx
                } else {
                    0
                };
                let prev_state = prev.entry(id.clone()).or_default();
                let bytes_delta = cur_total.saturating_sub(prev_state.total_bytes);
                let raw_bps = bytes_delta as f64 / dt;
                // EMA smoothing — the value reported in the snapshot is the
                // smoothed reading, but the "active" detection in the renderer
                // uses last_change_at so a single packet still wakes the wheel.
                prev_state.smoothed_bps =
                    BPS_EMA_ALPHA * raw_bps + (1.0 - BPS_EMA_ALPHA) * prev_state.smoothed_bps;
                if bytes_delta > 0 {
                    prev_state.last_change_at = Some(now);
                }
                prev_state.total_bytes = cur_total;
                let last_active_ms = prev_state
                    .last_change_at
                    .map(|t| now.duration_since(t).as_millis() as f64)
                    .unwrap_or(f64::INFINITY);
                let destinations = conns_by_chain.remove(id).unwrap_or_default();
                chains_view.push(ChainView {
                    id: id.clone(),
                    hops: hops.clone(),
                    bytes_per_sec: prev_state.smoothed_bps,
                    last_active_ms,
                    destinations,
                });
            }
            // Surface chains we got from /metrics that aren't in client-config
            // (e.g. dynamically added chains, or "__bypass__").
            if let Some(m) = metrics.as_ref() {
                for (id, cur) in &m.chains {
                    if seen.contains(id) { continue; }
                    let cur_total = cur.bytes_tx + cur.bytes_rx;
                    let prev_state = prev.entry(id.clone()).or_default();
                    let bytes_delta = cur_total.saturating_sub(prev_state.total_bytes);
                    let raw_bps = bytes_delta as f64 / dt;
                    prev_state.smoothed_bps =
                        BPS_EMA_ALPHA * raw_bps + (1.0 - BPS_EMA_ALPHA) * prev_state.smoothed_bps;
                    if bytes_delta > 0 { prev_state.last_change_at = Some(now); }
                    prev_state.total_bytes = cur_total;
                    let last_active_ms = prev_state
                        .last_change_at
                        .map(|t| now.duration_since(t).as_millis() as f64)
                        .unwrap_or(f64::INFINITY);
                    let destinations = conns_by_chain.remove(id).unwrap_or_default();
                    chains_view.push(ChainView {
                        id: id.clone(),
                        hops: Vec::new(),
                        bytes_per_sec: prev_state.smoothed_bps,
                        last_active_ms,
                        destinations,
                    });
                }
            }

            // Stable order — by id — so the renderer doesn't reshuffle on every poll.
            chains_view.sort_by(|a, b| a.id.cmp(&b.id));

            let snap = MeshSnapshot { chains: chains_view };
            // Best-effort send; if no receivers we'll just continue polling.
            let _ = tx.send(snap);
        }
    });

    rx
}
