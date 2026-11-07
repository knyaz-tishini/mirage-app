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
use std::path::{Path, PathBuf};
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
    /// Dial address — internal node identity only, used to key grid
    /// position and de-duplicate a node shared by several chains. NEVER
    /// rendered: it is the operator's own routing address, not something a
    /// customer's launcher may show them. Use [`Self::label`] for display.
    pub addr: String,              // e.g. "203.0.113.16:4700"
    pub role: HopRole,
    /// Neutral, customer-safe display label: a country code plus an index
    /// within that country (e.g. `"NL-1"`) when the mesh's peer-table
    /// gossip names one, or role plus index when no country is known
    /// (e.g. `"Relay-2"`). Never a hoster-derived name and never a raw
    /// address — this is the only hop text the renderer may draw.
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HopRole { Entry, Middle, Exit }

impl HopRole {
    /// Neutral word used as the label fallback bucket when no country tag
    /// is known for a hop, and as the human-readable name of the hop's
    /// position in the chain generally.
    pub fn word(self) -> &'static str {
        match self {
            HopRole::Entry => "Entry",
            HopRole::Middle => "Relay",
            HopRole::Exit => "Exit",
        }
    }
}

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
    /// Mesh node id (`mirage_core::config::HopConfig::node_id`) — the
    /// lookup key into the peer-table country tags, preferred over `addr`.
    #[serde(default)]
    node_id: Option<String>,
}

/// One hop resolved out of `client-config.json`, before country/index
/// labelling — an intermediate shape so the label pass can see every hop of
/// every chain (in on-disk array order — deterministic, no hashing) before
/// any [`HopView`] is built.
struct RawHop {
    addr: String,
    role: HopRole,
    node_id: Option<String>,
}

fn hops_from_chain(c: &ChainCfg) -> Vec<RawHop> {
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
            hops.push(RawHop { addr: h.addr.clone(), role, node_id: h.node_id.clone() });
        }
    } else {
        // Schema B: flat l1/l2 fields (legacy).
        if let Some(l1) = &c.l1 {
            hops.push(RawHop { addr: l1.addr.clone(), role: HopRole::Entry, node_id: l1.node_id.clone() });
        }
        if let Some(l2) = &c.l2 {
            hops.push(RawHop { addr: l2.addr.clone(), role: HopRole::Exit, node_id: l2.node_id.clone() });
        }
    }
    hops
}

// ── last-peer-table.json parsing — country tags only ─────────────────────────
//
// Sentinel persists `mirage_core::mirage::mesh::PeerTable` verbatim
// (`{"generation": N, "peers": [{"node_id", "addr", "labels": [...], ...}]}`)
// to `${state_dir}/last-peer-table.json` on every config push (l0-sentinel's
// R6 cold-start seed, `spawn_frame_watcher`'s `Config` arm). `labels` is
// documented as "free-form region labels (NL / TR / RU / ch / ...)" on
// `PeerTableEntry` — there is no dedicated `region`/`country` field anywhere
// on the wire (`HopConfig`, the per-hop shape actually materialised into
// `client-config.json`, carries none either). A label is treated as a
// country tag using the same rule `subscriber-client::artefact::validate`
// uses for `ArtefactMode::Country`: exactly two ASCII letters.

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawPeerTableFile {
    peers: Vec<RawPeerTableEntry>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawPeerTableEntry {
    node_id: String,
    addr: String,
    labels: Vec<String>,
}

/// The first `labels` entry that looks like a two-letter country code,
/// upper-cased. `None` when no such label is present — callers must fall
/// back to role, never invent a country.
fn country_from_labels(labels: &[String]) -> Option<String> {
    labels
        .iter()
        .find(|l| l.len() == 2 && l.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|l| l.to_uppercase())
}

/// Read `${state_dir}/last-peer-table.json` and return `(by_node_id,
/// by_addr)` country lookup maps. Tolerant of a missing file (fresh
/// install, or a mesh whose gossip carries no region labels yet) and of any
/// parse error — both fall back to empty maps, which in turn makes every
/// hop fall back to its role-based label.
fn load_country_tags(state_dir: &Path) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut by_node_id = HashMap::new();
    let mut by_addr = HashMap::new();

    let raw = match std::fs::read_to_string(state_dir.join("last-peer-table.json")) {
        Ok(s) => s,
        Err(_) => return (by_node_id, by_addr),
    };
    let file: RawPeerTableFile = match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(_) => return (by_node_id, by_addr),
    };

    for p in file.peers {
        let Some(cc) = country_from_labels(&p.labels) else { continue };
        if !p.node_id.is_empty() {
            by_node_id.insert(p.node_id, cc.clone());
        }
        if !p.addr.is_empty() {
            by_addr.insert(p.addr, cc);
        }
    }
    (by_node_id, by_addr)
}

/// Resolve the state directory the same way sentinel does
/// (`l0-sentinel::debug_shell::resolve_state_dir`): `sentinel.toml#state_dir`
/// when present, otherwise the directory `client-config.json` itself lives
/// in. Mirrors `model.rs::resolve_paths` — kept as its own small copy rather
/// than a shared helper, matching this crate's existing per-consumer
/// resolution style.
fn resolve_state_dir(exe_dir: &Path) -> PathBuf {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct SentinelTomlPartial {
        state_dir: Option<PathBuf>,
    }

    let partial: SentinelTomlPartial = std::fs::read_to_string(exe_dir.join("sentinel.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();

    partial.state_dir.unwrap_or_else(|| exe_dir.to_path_buf())
}

fn load_chain_topology(config_path: &Path) -> HashMap<String, Vec<HopView>> {
    let raw = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    let cfg: ClientConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let exe_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let state_dir = resolve_state_dir(exe_dir);
    let (country_by_node_id, country_by_addr) = load_country_tags(&state_dir);

    let per_chain_hops: Vec<(String, Vec<RawHop>)> = cfg
        .chains
        .iter()
        .map(|c| (c.chain_id.clone(), hops_from_chain(c)))
        .collect();

    // Pass 1: assign a stable neutral label per unique addr, first-seen
    // (on-disk array) order — the same physical node always gets the same
    // label for the lifetime of this poller (topology is loaded once, at
    // startup, same as before this change).
    let mut label_by_addr: HashMap<String, String> = HashMap::new();
    let mut next_index: HashMap<String, u32> = HashMap::new();
    for (_, hops) in &per_chain_hops {
        for h in hops {
            if label_by_addr.contains_key(&h.addr) {
                continue;
            }
            let bucket = h
                .node_id
                .as_deref()
                .and_then(|nid| country_by_node_id.get(nid))
                .or_else(|| country_by_addr.get(&h.addr))
                .cloned()
                .unwrap_or_else(|| h.role.word().to_string());
            let idx = next_index.entry(bucket.clone()).or_insert(0);
            *idx += 1;
            label_by_addr.insert(h.addr.clone(), format!("{bucket}-{idx}"));
        }
    }

    // Pass 2: build the render-safe view — addr survives only as the
    // internal identity key, label is the only text.
    let mut out = HashMap::new();
    for (chain_id, hops) in per_chain_hops {
        let views = hops
            .into_iter()
            .map(|h| {
                let label = label_by_addr
                    .get(&h.addr)
                    .cloned()
                    .unwrap_or_else(|| h.role.word().to_string());
                HopView { addr: h.addr, role: h.role, label }
            })
            .collect();
        out.insert(chain_id, views);
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
    // Demo labels only — illustrative country-index/role-index text, never
    // a hoster name (mirrors what `load_chain_topology`'s labelling pass
    // would produce from real peer-table country tags).
    let hop = |addr: &str, role: HopRole, label: &str| HopView {
        addr: addr.to_string(),
        role,
        label: label.to_string(),
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
                    hop("203.0.113.16:4700", HopRole::Entry, "RU-1"),
                    hop("203.0.113.14:6700", HopRole::Exit, "TR-1"),
                ],
                osc(0.0, 180_000.0),
                vec![
                    dest("203.0.113.135", 443, "github.com", 820_000),
                    dest("203.0.113.171", 443, "cloudflare.com", 410_000),
                ],
            ),
            chain(
                "auto-eu-onion",
                vec![
                    hop("203.0.113.11:4700", HopRole::Entry, "NL-1"),
                    hop("203.0.113.17:5700", HopRole::Middle, "CH-1"),
                    hop("203.0.113.14:6701", HopRole::Exit, "TR-2"),
                ],
                osc(2.1, 90_000.0),
                vec![dest("203.0.113.162", 443, "google.com", 350_000)],
            ),
            chain(
                "auto-ru-quic",
                vec![
                    hop("203.0.113.15:4700", HopRole::Entry, "RU-2"),
                    hop("203.0.113.17:6700", HopRole::Exit, "CH-2"),
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
                v.sort_by_key(|d| std::cmp::Reverse(d.bytes));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_from_labels_finds_two_letter_code_case_insensitively() {
        assert_eq!(
            country_from_labels(&["monitor".to_string(), "nl".to_string()]),
            Some("NL".to_string())
        );
        assert_eq!(country_from_labels(&["RU".to_string()]), Some("RU".to_string()));
        assert_eq!(country_from_labels(&["monitor".to_string()]), None);
        assert_eq!(country_from_labels(&[]), None);
    }

    /// End-to-end through the public entry point: a hop whose `node_id`
    /// resolves to a peer-table country label gets `"NL-1"`; a hop absent
    /// from the peer table falls back to role + index (`"Exit-1"`) — never
    /// the raw `addr`.
    #[test]
    fn load_chain_topology_labels_by_country_then_role_fallback() {
        let dir = std::env::temp_dir().join(format!(
            "mirage-launcher-mesh-state-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let config_path = dir.join("client-config.json");
        std::fs::write(
            &config_path,
            r#"{
                "chains": [
                    {
                        "chain_id": "c1",
                        "hops": [
                            {"addr": "1.1.1.1:1", "role": "entry", "node_id": "n1"},
                            {"addr": "203.0.113.14:2", "role": "exit", "node_id": "n2"}
                        ]
                    }
                ]
            }"#,
        )
        .expect("write client-config.json");
        std::fs::write(
            dir.join("last-peer-table.json"),
            r#"{
                "generation": 1,
                "peers": [
                    {"node_id": "n1", "addr": "1.1.1.1:1", "labels": ["monitor", "nl"]}
                ]
            }"#,
        )
        .expect("write last-peer-table.json");

        let topology = load_chain_topology(&config_path);
        let hops = topology.get("c1").expect("chain c1 present");
        assert_eq!(hops[0].addr, "1.1.1.1:1");
        assert_eq!(hops[0].label, "NL-1");
        assert_eq!(hops[1].addr, "203.0.113.14:2");
        assert_eq!(hops[1].label, "Exit-1");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
