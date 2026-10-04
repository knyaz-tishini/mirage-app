//! Unified `ClientState` model — aggregates all backend data sources into a
//! single richly-typed snapshot that the (future) visual layer can render.
//!
//! ## Data sources polled every ~500 ms
//!
//! 1. `GET 127.0.0.1:19420/status` — sentinel phase, uptime, child pids,
//!    monitor reachability.
//! 2. `${state_dir}/client-state.json` — per-chain health/latency + credential
//!    summary written by sentinel on every config materialise.
//! 3. `${state_dir}/last-peer-table.json` — mesh inventory (nodes by node_id).
//! 4. `client-config-managed.json` — full chain topology (hops, protocol,
//!    camouflage, routing, …).
//! 5. `GET 127.0.0.1:19485/metrics` — per-chain byte counters.
//! 6. `GET 127.0.0.1:19485/connections` — per-flow connection list.
//!
//! The poller merges all sources by `chain_id` and publishes into a
//! `watch::Receiver<Arc<ClientState>>`.  The existing `mesh_state` + sentinel
//! pollers are untouched; this module adds alongside them.
//!
//! ## Mock fixture
//!
//! Set `MIRAGE_LAUNCHER_MOCK=1` to make the poller serve a static realistic
//! snapshot instead of hitting the network — useful for building/reviewing the
//! visual layer without a running backend.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

// ── Poll cadence ──────────────────────────────────────────────────────────────

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// EMA alpha for bytes-per-second smoothing — mirrors mesh_state.rs.
const BPS_EMA_ALPHA: f64 = 0.45;

// ── Public snapshot types ─────────────────────────────────────────────────────

/// Top-level unified snapshot. All fields are `Clone` so the renderer can
/// cheaply take a frame copy without holding locks.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ClientState {
    pub conn: ConnState,
    pub credential: Option<CredentialState>,
    pub chains: Vec<ChainState>,
    pub mesh: MeshState,
    pub routing: RoutingState,
    pub flows: Vec<FlowState>,
}

/// Connectivity / sentinel status.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ConnState {
    /// Discriminant string from sentinel (`"starting"`, `"config_ready"`, …).
    pub phase: String,
    /// Optional structured detail extracted from the phase variant.
    pub phase_detail: PhaseDetail,
    pub monitor_url: String,
    pub monitor_reachable: bool,
    pub uptime_ms: u64,
    pub tun_alive: bool,
    pub tun_pid: Option<u32>,
    pub cli_alive: bool,
    pub cli_pid: Option<u32>,
}

/// Structured data from the active sentinel phase variant.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PhaseDetail {
    // ConfigReady
    pub config_version: Option<u64>,
    pub config_chains: Option<usize>,
    pub materialised_at_ms: Option<u64>,
    // ConfigStale
    pub stale_since_ms: Option<u64>,
    // FetchingConfig
    pub fetching_monitor_url: Option<String>,
    // Error
    pub error_message: Option<String>,
    // ChildRunning
    pub child_pid: Option<u32>,
}

/// Active credential scheme reported by sentinel.
#[derive(Clone, Debug, Serialize)]
pub struct CredentialState {
    pub scheme: String,
    pub generation: u64,
}

/// Per-chain merged view: topology from config + health from client-state.json
/// + throughput from /metrics + connections from /connections.
#[derive(Clone, Debug, Serialize)]
pub struct ChainState {
    pub chain_id: String,

    // From client-state.json
    pub healthy: Option<bool>,
    pub latency_ms: Option<u64>,

    // From client-config-managed.json
    pub hops: Vec<HopState>,
    pub protocol: Option<String>,
    pub inner_camo: Option<String>,
    pub aead: Option<String>,
    pub kill_switch: Option<bool>,
    pub group: Option<String>,
    pub priority: Option<i64>,

    // From /metrics (EMA-smoothed)
    pub bytes_per_sec: f64,
    pub connections: u64,
    pub errors: u64,
}

/// Single hop within a chain.
#[derive(Clone, Debug, Serialize)]
pub struct HopState {
    pub role: String,
    pub node_id: Option<String>,
    pub display_name: Option<String>,
    pub region: Option<String>,
    pub addr: String,
    pub camouflage: Option<String>,
}

/// Mesh peer inventory from `last-peer-table.json`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MeshState {
    pub nodes: Vec<MeshNode>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MeshNode {
    pub node_id: String,
    pub region: Option<String>,
    pub labels: Vec<String>,
    pub addr: Option<String>,
}

/// Routing/bypass state merged from config + /metrics bypass counters.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RoutingState {
    pub direct_cidrs: Vec<String>,
    pub blocked_domains: Vec<String>,
    pub direct_domains: Vec<String>,
    pub bypass_bytes_per_sec: f64,
    pub rule_hits: Vec<RuleHit>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RuleHit {
    pub index: usize,
    pub hits: u64,
}

/// Single active flow (connection).
#[derive(Clone, Debug, Serialize)]
pub struct FlowState {
    pub chain: String,
    pub domain: Option<String>,
    pub dest_ip: String,
    pub dest_port: u16,
    pub proto: Option<String>,
    pub pid: Option<u32>,
    pub exe_path: Option<String>,
    pub bytes_per_sec: f64,
}

// ── Raw JSON shapes from sentinel /status ─────────────────────────────────────

/// Tolerant superset — we only need a subset of the full StatusResponse.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawStatus {
    phase: String,
    // Phase-variant fields (all optional — depends on discriminant)
    version: Option<u64>,
    chains: Option<usize>,
    materialised_at_ms: Option<u64>,
    since_ms: Option<u64>,
    monitor_url_field: Option<String>, // "fetching_config" variant field name
    message: Option<String>,
    pid: Option<u32>,

    uptime_ms: Option<u64>,
    cli: Option<RawChildStatus>,
    tun: Option<RawChildStatus>,
    monitor_url: Option<String>,
    monitor_reachable: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawChildStatus {
    alive: bool,
    pid: Option<u32>,
}

// ── Raw JSON shapes from client-state.json ────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawClientStateFile {
    chains: Vec<RawClientStateChain>,
    credential: Option<RawCredentialSummary>,
    #[allow(dead_code)]
    updated_at_ms: u64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawClientStateChain {
    chain_id: String,
    healthy: Option<bool>,
    latency_ms: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawCredentialSummary {
    scheme: String,
    generation: u64,
}

// ── Raw JSON shapes from last-peer-table.json ─────────────────────────────────
//
// The peer table is an opaque JSON blob from the monitor; shape is:
// { "<node_id>": { "addr": "...", "labels": [...], "region": "..." } }
// Parse tolerantly.

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawPeerEntry {
    addr: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    region: Option<String>,
}

// ── Raw JSON shapes from client-config-managed.json ───────────────────────────

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawClientConfig {
    #[allow(dead_code)]
    client_id: Option<String>,
    #[allow(dead_code)]
    mesh_id: Option<String>,
    chains: Vec<RawChainCfg>,
    routing: Option<RawRoutingCfg>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawChainCfg {
    chain_id: String,
    hops: Vec<RawHopCfg>,
    // Legacy flat schema
    l1: Option<RawHopCfg>,
    l2: Option<RawHopCfg>,
    // Protocol field — may be a string ("Mirage") or a tagged object
    #[serde(deserialize_with = "deser_protocol_field")]
    protocol: Option<String>,
    // Various optional per-chain fields
    inner_camouflage: Option<serde_json::Value>,
    camouflage: Option<serde_json::Value>,
    aead_suite: Option<String>,
    kill_switch: Option<bool>,
    group: Option<String>,
    priority: Option<i64>,
}

fn deser_protocol_field<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<serde_json::Value> = Option::deserialize(de)?;
    Ok(v.map(|val| match val {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawHopCfg {
    addr: String,
    role: Option<String>,
    node_id: Option<String>,
    display_name: Option<String>,
    region: Option<String>,
    camouflage: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawRoutingCfg {
    direct_cidrs: Vec<String>,
    blocked_domains: Vec<String>,
    direct_domains: Vec<String>,
}

// ── Raw JSON shapes from /metrics ─────────────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawMetrics {
    chains: HashMap<String, RawChainMetric>,
    bypass: Option<RawChainMetric>,
    rules: Vec<RawRuleMetric>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct RawChainMetric {
    connections: Option<u64>,
    bytes_tx: u64,
    bytes_rx: u64,
    errors: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawRuleMetric {
    index: usize,
    hits: u64,
}

// ── Raw JSON shapes from /connections ─────────────────────────────────────────
//
// Tolerant superset — Windows has pid/exe_path, macOS does not.

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawFlow {
    chain: String,
    dest_ip: String,
    dest_port: u16,
    domain: Option<String>,
    proto: Option<String>,
    pid: Option<u32>,
    exe_path: Option<String>,
    bytes_tx: Option<u64>,
    bytes_rx: Option<u64>,
    // macOS fields
    #[allow(dead_code)]
    id: Option<String>,
    #[allow(dead_code)]
    state: Option<String>,
    // Windows/macOS age fields
    #[allow(dead_code)]
    age_ms: Option<u64>,
    #[allow(dead_code)]
    idle_ms: Option<u64>,
}

// ── State directory resolution ─────────────────────────────────────────────────
//
// Mirrors what sentinel's `debug_shell::resolve_state_dir` does:
// - If `sentinel.toml` contains `state_dir`, use that.
// - Otherwise fall back to exe_dir (same dir as client-config.json).
//
// We also look for `client-config-managed.json` alongside the exe; if sentinel
// uses a different `client_config_path`, we read that from sentinel.toml too.

#[derive(Deserialize, Default)]
#[serde(default)]
struct SentinelTomlPartial {
    state_dir: Option<PathBuf>,
    client_config_path: Option<PathBuf>,
}

fn resolve_paths(exe_dir: &Path) -> (PathBuf, PathBuf) {
    let toml_path = exe_dir.join("sentinel.toml");
    let partial: SentinelTomlPartial = std::fs::read_to_string(&toml_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();

    let state_dir = partial
        .state_dir
        .unwrap_or_else(|| exe_dir.to_path_buf());

    let managed_cfg = partial
        .client_config_path
        .unwrap_or_else(|| exe_dir.join("client-config-managed.json"));

    (state_dir, managed_cfg)
}

// ── Poller internal state ─────────────────────────────────────────────────────

#[derive(Default)]
struct ChainBpsState {
    total_bytes: u64,
    last_change: Option<Instant>,
    smoothed_bps: f64,
}

struct BypassBpsState {
    total_bytes: u64,
    smoothed_bps: f64,
}

impl Default for BypassBpsState {
    fn default() -> Self {
        Self { total_bytes: 0, smoothed_bps: 0.0 }
    }
}

// ── Public poller ─────────────────────────────────────────────────────────────

/// Spawn the model poller. Returns a `watch::Receiver` that the render loop
/// (and future visual layer) can call `.borrow()` on every frame.
///
/// True when the launcher runs in the offline mock sandbox
/// (`MIRAGE_LAUNCHER_MOCK=1`): no real backend is spawned and every poller
/// serves synthetic data so the GUI behaves as if connected to a live mesh.
pub fn mock_enabled() -> bool {
    std::env::var("MIRAGE_LAUNCHER_MOCK").as_deref() == Ok("1")
}

/// If `MIRAGE_LAUNCHER_MOCK=1` is set the poller immediately publishes
/// `ClientState::mock()` and never polls the network.
pub fn spawn_model_poller(
    rt: &tokio::runtime::Handle,
    exe_dir: PathBuf,
) -> watch::Receiver<Arc<ClientState>> {
    let initial = Arc::new(ClientState::default());
    let (tx, rx) = watch::channel(initial);

    if mock_enabled() {
        let mock = Arc::new(ClientState::mock());
        let _ = tx.send(mock);
        // Keep the sender alive so the receiver never sees a closed channel.
        rt.spawn(async move {
            let _tx = tx;
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        return rx;
    }

    rt.spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("reqwest client always builds");

        let mut chain_bps: HashMap<String, ChainBpsState> = HashMap::new();
        let mut bypass_bps = BypassBpsState::default();
        let mut last_poll = Instant::now();

        loop {
            tokio::time::sleep(POLL_INTERVAL).await;

            let (state_dir, managed_cfg_path) = resolve_paths(&exe_dir);

            // Fire all network requests in parallel.
            let (status_res, metrics_res, conns_res) = tokio::join!(
                client.get("http://127.0.0.1:19420/status").send(),
                client.get("http://127.0.0.1:19485/metrics").send(),
                client.get("http://127.0.0.1:19485/connections").send(),
            );

            let now = Instant::now();
            let dt = now.duration_since(last_poll).as_secs_f64().max(0.001);
            last_poll = now;

            // 1. Parse sentinel /status.
            let raw_status: RawStatus = match status_res {
                Ok(r) if r.status().is_success() => {
                    r.json().await.unwrap_or_default()
                }
                _ => RawStatus::default(),
            };

            // 2. Read client-state.json from disk (blocking read on async thread
            //    is acceptable — it's a tiny file on local SSD; tokio's small
            //    file read is faster than spawning a blocking task).
            let client_state_file: RawClientStateFile = {
                let path = state_dir.join("client-state.json");
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default()
            };

            // 3. Read last-peer-table.json.
            let peer_table_raw: HashMap<String, RawPeerEntry> = {
                let path = state_dir.join("last-peer-table.json");
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default()
            };

            // 4. Read client-config-managed.json.
            let raw_cfg: RawClientConfig = {
                std::fs::read_to_string(&managed_cfg_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default()
            };

            // 5. Parse /metrics.
            let raw_metrics: RawMetrics = match metrics_res {
                Ok(r) if r.status().is_success() => {
                    r.json().await.unwrap_or_default()
                }
                _ => RawMetrics::default(),
            };

            // 6. Parse /connections.
            let raw_flows: Vec<RawFlow> = match conns_res {
                Ok(r) if r.status().is_success() => {
                    r.json().await.unwrap_or_default()
                }
                _ => Vec::new(),
            };

            // ── Build ConnState ────────────────────────────────────────────────
            let phase_detail = build_phase_detail(&raw_status);
            let conn = ConnState {
                phase: raw_status.phase.clone(),
                phase_detail,
                monitor_url: raw_status.monitor_url.unwrap_or_default(),
                monitor_reachable: raw_status.monitor_reachable.unwrap_or(false),
                uptime_ms: raw_status.uptime_ms.unwrap_or(0),
                tun_alive: raw_status.tun.as_ref().map(|t| t.alive).unwrap_or(false),
                tun_pid: raw_status.tun.as_ref().and_then(|t| t.pid),
                cli_alive: raw_status.cli.as_ref().map(|c| c.alive).unwrap_or(false),
                cli_pid: raw_status.cli.as_ref().and_then(|c| c.pid),
            };

            // ── Credential ─────────────────────────────────────────────────────
            let credential = client_state_file.credential.map(|c| CredentialState {
                scheme: c.scheme,
                generation: c.generation,
            });

            // ── Health index from client-state.json ────────────────────────────
            let mut health_by_id: HashMap<String, (Option<bool>, Option<u64>)> = HashMap::new();
            for c in &client_state_file.chains {
                health_by_id.insert(c.chain_id.clone(), (c.healthy, c.latency_ms));
            }

            // ── Metrics — update EMA per chain ────────────────────────────────
            for (id, m) in &raw_metrics.chains {
                let total = m.bytes_tx + m.bytes_rx;
                let state = chain_bps.entry(id.clone()).or_default();
                let delta = total.saturating_sub(state.total_bytes);
                let raw_bps = delta as f64 / dt;
                state.smoothed_bps =
                    BPS_EMA_ALPHA * raw_bps + (1.0 - BPS_EMA_ALPHA) * state.smoothed_bps;
                if delta > 0 {
                    state.last_change = Some(now);
                }
                state.total_bytes = total;
            }
            // Bypass EMA.
            if let Some(byp) = &raw_metrics.bypass {
                let total = byp.bytes_tx + byp.bytes_rx;
                let delta = total.saturating_sub(bypass_bps.total_bytes);
                let raw_bps = delta as f64 / dt;
                bypass_bps.smoothed_bps =
                    BPS_EMA_ALPHA * raw_bps + (1.0 - BPS_EMA_ALPHA) * bypass_bps.smoothed_bps;
                bypass_bps.total_bytes = total;
            }

            // ── Build chain states — merge config + health + metrics ───────────
            let chains = build_chains(
                &raw_cfg.chains,
                &health_by_id,
                &raw_metrics.chains,
                &chain_bps,
            );

            // ── MeshState from peer table ──────────────────────────────────────
            let mesh_nodes: Vec<MeshNode> = peer_table_raw
                .into_iter()
                .map(|(node_id, entry)| MeshNode {
                    node_id,
                    region: entry.region,
                    labels: entry.labels,
                    addr: entry.addr,
                })
                .collect();
            let mesh = MeshState { nodes: mesh_nodes };

            // ── RoutingState ───────────────────────────────────────────────────
            let (direct_cidrs, blocked_domains, direct_domains) =
                raw_cfg.routing.map(|r| (r.direct_cidrs, r.blocked_domains, r.direct_domains))
                    .unwrap_or_default();
            let rule_hits = raw_metrics.rules.iter()
                .map(|r| RuleHit { index: r.index, hits: r.hits })
                .collect();
            let routing = RoutingState {
                direct_cidrs,
                blocked_domains,
                direct_domains,
                bypass_bytes_per_sec: bypass_bps.smoothed_bps,
                rule_hits,
            };

            // ── FlowState from /connections ────────────────────────────────────
            // Diff-based per-flow bps is not practical without stable flow IDs;
            // we expose the instantaneous window (bytes_tx+rx) / dt as a best-
            // effort rate. Callers that want cumulative bytes should use /metrics.
            let flows: Vec<FlowState> = raw_flows
                .into_iter()
                .map(|f| {
                    let total = f.bytes_tx.unwrap_or(0) + f.bytes_rx.unwrap_or(0);
                    FlowState {
                        chain: f.chain,
                        domain: f.domain,
                        dest_ip: f.dest_ip,
                        dest_port: f.dest_port,
                        proto: f.proto,
                        pid: f.pid,
                        exe_path: f.exe_path,
                        bytes_per_sec: total as f64 / dt,
                    }
                })
                .collect();

            let snapshot = Arc::new(ClientState {
                conn,
                credential,
                chains,
                mesh,
                routing,
                flows,
            });

            let _ = tx.send(snapshot);
        }
    });

    rx
}

// ── Phase detail helper ───────────────────────────────────────────────────────

fn build_phase_detail(raw: &RawStatus) -> PhaseDetail {
    PhaseDetail {
        config_version: if raw.phase == "config_ready" { raw.version } else { None },
        config_chains: if raw.phase == "config_ready" { raw.chains } else { None },
        materialised_at_ms: if raw.phase == "config_ready" { raw.materialised_at_ms } else { None },
        stale_since_ms: if raw.phase == "config_stale" { raw.since_ms } else { None },
        fetching_monitor_url: if raw.phase == "fetching_config" {
            raw.monitor_url_field.clone().or_else(|| raw.monitor_url.clone())
        } else {
            None
        },
        error_message: if raw.phase == "error" { raw.message.clone() } else { None },
        child_pid: if raw.phase == "child_running" { raw.pid } else { None },
    }
}

// ── Chain builder ─────────────────────────────────────────────────────────────

fn build_chains(
    raw_chains: &[RawChainCfg],
    health_by_id: &HashMap<String, (Option<bool>, Option<u64>)>,
    metrics: &HashMap<String, RawChainMetric>,
    bps_state: &HashMap<String, ChainBpsState>,
) -> Vec<ChainState> {
    raw_chains.iter().map(|c| {
        let (healthy, latency_ms) = health_by_id
            .get(&c.chain_id)
            .copied()
            .unwrap_or((None, None));

        let m = metrics.get(&c.chain_id);
        let connections = m.and_then(|x| x.connections).unwrap_or(0);
        let errors = m.and_then(|x| x.errors).unwrap_or(0);
        let bytes_per_sec = bps_state
            .get(&c.chain_id)
            .map(|s| s.smoothed_bps)
            .unwrap_or(0.0);

        // Build hops — prefer explicit `hops[]`, fall back to flat l1/l2.
        let hops = if !c.hops.is_empty() {
            c.hops.iter().map(hop_from_raw).collect()
        } else {
            let mut v = Vec::new();
            if let Some(l1) = &c.l1 {
                v.push(hop_from_raw_with_role(l1, "entry"));
            }
            if let Some(l2) = &c.l2 {
                v.push(hop_from_raw_with_role(l2, "exit"));
            }
            v
        };

        // Outer camouflage — take from first hop if present, or chain-level.
        let inner_camo = c.inner_camouflage.as_ref()
            .or(c.camouflage.as_ref())
            .map(|v| v.to_string());

        ChainState {
            chain_id: c.chain_id.clone(),
            healthy,
            latency_ms,
            hops,
            protocol: c.protocol.clone(),
            inner_camo,
            aead: c.aead_suite.clone(),
            kill_switch: c.kill_switch,
            group: c.group.clone(),
            priority: c.priority,
            bytes_per_sec,
            connections,
            errors,
        }
    }).collect()
}

fn hop_from_raw(h: &RawHopCfg) -> HopState {
    let role = h.role.clone().unwrap_or_else(|| "middle".to_string());
    hop_from_raw_with_role(h, &role)
}

fn hop_from_raw_with_role(h: &RawHopCfg, role: &str) -> HopState {
    HopState {
        role: role.to_string(),
        node_id: h.node_id.clone(),
        display_name: h.display_name.clone(),
        region: h.region.clone(),
        addr: h.addr.clone(),
        camouflage: h.camouflage.as_ref().map(|v| v.to_string()),
    }
}

// ── Mock fixture ──────────────────────────────────────────────────────────────

impl ClientState {
    /// Realistic mock snapshot for visual development without a running backend.
    ///
    /// Enable with `MIRAGE_LAUNCHER_MOCK=1`.
    pub fn mock() -> Self {
        ClientState {
            conn: ConnState {
                phase: "child_running".to_string(),
                phase_detail: PhaseDetail {
                    child_pid: Some(12345),
                    ..Default::default()
                },
                monitor_url: "https://lm-nl.zen-geldmaschine.net:19443".to_string(),
                monitor_reachable: true,
                uptime_ms: 120_000,
                tun_alive: true,
                tun_pid: Some(12346),
                cli_alive: true,
                cli_pid: Some(12347),
            },
            credential: Some(CredentialState {
                scheme: "ring_membership".to_string(),
                generation: 7,
            }),
            chains: vec![
                ChainState {
                    chain_id: "chain-nl-01".to_string(),
                    healthy: Some(true),
                    latency_ms: Some(42),
                    hops: vec![
                        HopState {
                            role: "entry".to_string(),
                            node_id: Some("v2-nl-hostkey-091.12".to_string()),
                            display_name: Some("NL-Entry".to_string()),
                            region: Some("eu-nl".to_string()),
                            addr: "91.218.245.12:4701".to_string(),
                            camouflage: None,
                        },
                        HopState {
                            role: "exit".to_string(),
                            node_id: Some("v2-ru-hostkey-094.74".to_string()),
                            display_name: Some("RU-Exit".to_string()),
                            region: Some("ru".to_string()),
                            addr: "94.74.82.55:4700".to_string(),
                            camouflage: Some(r#"{"kind":"tls","sni":"cdn.example.com"}"#.to_string()),
                        },
                    ],
                    protocol: Some("Mirage".to_string()),
                    inner_camo: None,
                    aead: Some("aes_256_gcm".to_string()),
                    kill_switch: Some(true),
                    group: Some("primary".to_string()),
                    priority: Some(10),
                    bytes_per_sec: 48_500.0,
                    connections: 3,
                    errors: 0,
                },
                ChainState {
                    chain_id: "chain-de-02".to_string(),
                    healthy: Some(true),
                    latency_ms: Some(68),
                    hops: vec![
                        HopState {
                            role: "entry".to_string(),
                            node_id: Some("v2-de-hostkey-088.21".to_string()),
                            display_name: Some("DE-Entry".to_string()),
                            region: Some("eu-de".to_string()),
                            addr: "88.198.21.44:4702".to_string(),
                            camouflage: None,
                        },
                        HopState {
                            role: "middle".to_string(),
                            node_id: Some("v2-nl-hostkey-091.12".to_string()),
                            display_name: Some("NL-Mid".to_string()),
                            region: Some("eu-nl".to_string()),
                            addr: "91.218.245.12:4703".to_string(),
                            camouflage: None,
                        },
                        HopState {
                            role: "exit".to_string(),
                            node_id: Some("v2-kz-hostkey-095.33".to_string()),
                            display_name: Some("KZ-Exit".to_string()),
                            region: Some("kz".to_string()),
                            addr: "95.214.33.90:4700".to_string(),
                            camouflage: Some(r#"{"kind":"reality","server_name":"www.speedtest.net"}"#.to_string()),
                        },
                    ],
                    protocol: Some("Mirage".to_string()),
                    inner_camo: None,
                    aead: Some("chacha20_poly1305".to_string()),
                    kill_switch: Some(false),
                    group: Some("secondary".to_string()),
                    priority: Some(5),
                    bytes_per_sec: 2_100.0,
                    connections: 1,
                    errors: 0,
                },
                ChainState {
                    chain_id: "chain-us-03".to_string(),
                    healthy: Some(false),
                    latency_ms: None,
                    hops: vec![
                        HopState {
                            role: "entry".to_string(),
                            node_id: Some("v2-us-hostkey-072.19".to_string()),
                            display_name: Some("US-Entry".to_string()),
                            region: Some("us-east".to_string()),
                            addr: "72.44.19.101:4700".to_string(),
                            camouflage: None,
                        },
                    ],
                    protocol: Some("Mirage".to_string()),
                    inner_camo: None,
                    aead: Some("aes_256_gcm".to_string()),
                    kill_switch: Some(true),
                    group: Some("us".to_string()),
                    priority: Some(1),
                    bytes_per_sec: 0.0,
                    connections: 0,
                    errors: 2,
                },
            ],
            mesh: MeshState {
                nodes: vec![
                    MeshNode {
                        node_id: "v2-nl-hostkey-091.12".to_string(),
                        region: Some("eu-nl".to_string()),
                        labels: vec!["entry".to_string(), "exit".to_string()],
                        addr: Some("91.218.245.12:4701".to_string()),
                    },
                    MeshNode {
                        node_id: "v2-ru-hostkey-094.74".to_string(),
                        region: Some("ru".to_string()),
                        labels: vec!["exit".to_string()],
                        addr: Some("94.74.82.55:4700".to_string()),
                    },
                    MeshNode {
                        node_id: "v2-de-hostkey-088.21".to_string(),
                        region: Some("eu-de".to_string()),
                        labels: vec!["entry".to_string()],
                        addr: Some("88.198.21.44:4702".to_string()),
                    },
                    MeshNode {
                        node_id: "v2-kz-hostkey-095.33".to_string(),
                        region: Some("kz".to_string()),
                        labels: vec!["exit".to_string()],
                        addr: Some("95.214.33.90:4700".to_string()),
                    },
                    MeshNode {
                        node_id: "v2-us-hostkey-072.19".to_string(),
                        region: Some("us-east".to_string()),
                        labels: vec!["entry".to_string(), "exit".to_string()],
                        addr: Some("72.44.19.101:4700".to_string()),
                    },
                ],
            },
            routing: RoutingState {
                direct_cidrs: vec!["10.0.0.0/8".to_string(), "192.168.0.0/16".to_string()],
                blocked_domains: vec!["ads.example.com".to_string()],
                direct_domains: vec!["localhost".to_string()],
                bypass_bytes_per_sec: 1_200.0,
                rule_hits: vec![
                    RuleHit { index: 0, hits: 1042 },
                    RuleHit { index: 1, hits: 87 },
                ],
            },
            flows: vec![
                FlowState {
                    chain: "chain-nl-01".to_string(),
                    domain: Some("api.binance.com".to_string()),
                    dest_ip: "13.32.162.99".to_string(),
                    dest_port: 443,
                    proto: Some("tcp".to_string()),
                    pid: Some(9988),
                    exe_path: Some(r"C:\Program Files\Mirage\mirage-chain-worker.exe".to_string()),
                    bytes_per_sec: 12_300.0,
                },
                FlowState {
                    chain: "chain-nl-01".to_string(),
                    domain: Some("stream.binance.com".to_string()),
                    dest_ip: "52.84.10.120".to_string(),
                    dest_port: 9443,
                    proto: Some("tcp".to_string()),
                    pid: Some(9988),
                    exe_path: Some(r"C:\Program Files\Mirage\mirage-chain-worker.exe".to_string()),
                    bytes_per_sec: 36_200.0,
                },
                FlowState {
                    chain: "chain-de-02".to_string(),
                    domain: Some("coinbase.com".to_string()),
                    dest_ip: "104.16.8.5".to_string(),
                    dest_port: 443,
                    proto: Some("tcp".to_string()),
                    pid: None,
                    exe_path: None,
                    bytes_per_sec: 2_100.0,
                },
            ],
        }
    }
}
