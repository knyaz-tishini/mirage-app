//! Router trait + RouterImpl — dispatches flows to chains via RoutingPolicy.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;

use dashmap::DashMap;
use ipnet::{Ipv4Net, Ipv6Net};
use mirage_core::error::MirageError;

use super::handle::ChainHandle;
use super::types::ChainId;
use super::ChainSet;

// ── TargetKey ─────────────────────────────────────────────────────────────────

/// Describes the destination of an outbound flow.
#[derive(Debug, Clone)]
pub struct TargetKey {
    pub host: String,
    pub port: u16,
    pub protocol_hint: ProtocolHint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolHint {
    Tcp,
    Udp,
    Tls,
    Http,
    Quic,
    Unknown,
}

// ── FlowKey ───────────────────────────────────────────────────────────────────

/// Identifies a flow for sticky routing.
#[derive(Debug, Clone)]
pub enum FlowKey {
    Socks5 {
        client_addr: std::net::SocketAddr,
        request_id: u64,
    },
    TunFlow {
        five_tuple_hash: u64,
    },
    HttpConnect {
        connection_id: u64,
    },
}

impl FlowKey {
    pub fn stable_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match self {
            FlowKey::Socks5 {
                client_addr,
                request_id,
            } => {
                client_addr.hash(&mut h);
                request_id.hash(&mut h);
            }
            FlowKey::TunFlow { five_tuple_hash } => {
                five_tuple_hash.hash(&mut h);
            }
            FlowKey::HttpConnect { connection_id } => {
                connection_id.hash(&mut h);
            }
        }
        h.finish()
    }
}

// ── BypassMatcher ─────────────────────────────────────────────────────────────

/// Matcher for the `BypassFirst` routing policy.
#[derive(Clone, Debug)]
pub struct BypassMatcher {
    pub cidrs_v4: Vec<Ipv4Net>,
    pub cidrs_v6: Vec<Ipv6Net>,
    pub domains: Vec<String>,
    pub exact_ips: Vec<IpAddr>,
}

impl BypassMatcher {
    pub fn empty() -> Self {
        Self {
            cidrs_v4: Vec::new(),
            cidrs_v6: Vec::new(),
            domains: Vec::new(),
            exact_ips: Vec::new(),
        }
    }

    pub fn matches(&self, target: &TargetKey) -> bool {
        for pat in &self.domains {
            if target.host.ends_with(pat.as_str()) || target.host == pat.as_str() {
                return true;
            }
        }
        if let Ok(ip) = target.host.parse::<IpAddr>() {
            if self.exact_ips.contains(&ip) {
                return true;
            }
            match ip {
                IpAddr::V4(v4) => {
                    for net in &self.cidrs_v4 {
                        if net.contains(&v4) {
                            return true;
                        }
                    }
                }
                IpAddr::V6(v6) => {
                    for net in &self.cidrs_v6 {
                        if net.contains(&v6) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

// ── GeoTag ───────────────────────────────────────────────────────────────────

/// ISO 3166-1 alpha-2 country code or custom geographic label.
pub type GeoTag = String;

// ── RoutingPolicy ─────────────────────────────────────────────────────────────

/// Determines how `RouterImpl` dispatches flows to chains.
#[derive(Clone)]
pub enum RoutingPolicy {
    RoundRobin,
    LatencyWeighted {
        window_ms: u64,
        min_samples: u32,
    },
    StickyPerFlow {
        hash_seed: u64,
        ttl_secs: u64,
    },
    RegionAffinity {
        target_geo: HashMap<GeoTag, Vec<ChainId>>,
        fallback: Box<RoutingPolicy>,
    },
    BypassFirst {
        bypass: BypassMatcher,
        fallback: Box<RoutingPolicy>,
    },
}

// ── Router trait ──────────────────────────────────────────────────────────────

/// Public interface for all routing implementations.
///
/// `pick` returns an ordered list: index-0 = primary, rest = fallbacks.
/// Empty list means no chain available (caller should return a 503-equivalent).
///
/// Callers MUST call `record_success` / `record_failure` after every completed
/// or failed lease so LatencyWeighted and StickyPerFlow policies stay accurate.
pub trait Router: Send + Sync {
    fn pick(&self, target: &TargetKey, flow: &FlowKey) -> Vec<ChainId>;
    fn record_success(&self, chain_id: &ChainId, latency_ms: u64);
    fn record_failure(&self, chain_id: &ChainId, error: &MirageError);
}

// ── RouterImpl ────────────────────────────────────────────────────────────────

pub struct RouterImpl {
    chain_set: Arc<ChainSet>,
    policy: RoutingPolicy,
    rr_cursor: AtomicU64,
    sticky_map: DashMap<u64, (ChainId, Instant)>,
}

impl Router for RouterImpl {
    fn pick(&self, target: &TargetKey, flow: &FlowKey) -> Vec<ChainId> {
        self.dispatch(target, flow)
    }

    fn record_success(&self, chain_id: &ChainId, latency_ms: u64) {
        if let Some(h) = self.chain_set.chains.get(chain_id) {
            h.metrics.record_success(latency_ms);
        }
    }

    fn record_failure(&self, chain_id: &ChainId, _error: &MirageError) {
        if let Some(h) = self.chain_set.chains.get(chain_id) {
            h.metrics.record_failure();
        }
    }
}

impl RouterImpl {
    pub fn new(chain_set: Arc<ChainSet>, policy: RoutingPolicy) -> Arc<dyn Router> {
        Arc::new(Self {
            chain_set,
            policy,
            rr_cursor: AtomicU64::new(0),
            sticky_map: DashMap::new(),
        })
    }

    fn dispatch(&self, target: &TargetKey, flow: &FlowKey) -> Vec<ChainId> {
        match &self.policy {
            RoutingPolicy::RoundRobin => self.pick_round_robin(),
            RoutingPolicy::LatencyWeighted { min_samples, .. } => {
                self.pick_latency_weighted(*min_samples)
            }
            RoutingPolicy::StickyPerFlow {
                hash_seed,
                ttl_secs,
            } => self.pick_sticky(flow, *hash_seed, *ttl_secs),
            RoutingPolicy::RegionAffinity {
                target_geo,
                fallback,
            } => self.pick_region(target, target_geo, fallback),
            RoutingPolicy::BypassFirst { bypass, fallback } => {
                if bypass.matches(target) {
                    Vec::new() // empty = bypass (caller routes direct)
                } else {
                    let inner = RouterImpl {
                        chain_set: Arc::clone(&self.chain_set),
                        policy: (**fallback).clone(),
                        rr_cursor: AtomicU64::new(self.rr_cursor.load(Ordering::Relaxed)),
                        sticky_map: DashMap::new(),
                    };
                    inner.dispatch(target, flow)
                }
            }
        }
    }

    fn pick_round_robin(&self) -> Vec<ChainId> {
        let chains = self.chain_set.active_chains();
        if chains.is_empty() {
            return Vec::new();
        }
        let idx = self.rr_cursor.fetch_add(1, Ordering::Relaxed) as usize % chains.len();
        let primary = chains[idx].id.clone();
        let mut result = vec![primary];
        for (i, h) in chains.iter().enumerate() {
            if i != idx {
                result.push(h.id.clone());
            }
        }
        result
    }

    fn pick_latency_weighted(&self, min_samples: u32) -> Vec<ChainId> {
        let chains = self.chain_set.active_chains();
        if chains.is_empty() {
            return Vec::new();
        }

        use std::sync::atomic::Ordering::Relaxed;
        let mut scored: Vec<(f64, &Arc<ChainHandle>)> = chains
            .iter()
            .map(|h| {
                let p50 = h.metrics.p50_latency_ms.load(Relaxed);
                let samples = h.metrics.success_count.load(Relaxed)
                    + h.metrics.failure_count.load(Relaxed);
                let latency_score = if p50 == u64::MAX || p50 == 0 {
                    1.0
                } else {
                    1000.0 / p50 as f64
                };
                let exploration = if samples < min_samples as u64 { 0.5 } else { 0.0 };
                let score = latency_score * h.metrics.success_rate() + exploration;
                (score, h)
            })
            .collect();

        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.iter().map(|(_, h)| h.id.clone()).collect()
    }

    fn pick_sticky(&self, flow: &FlowKey, hash_seed: u64, ttl_secs: u64) -> Vec<ChainId> {
        let key = flow.stable_hash().wrapping_add(hash_seed);
        let now = Instant::now();

        if let Some(entry) = self.sticky_map.get(&key) {
            let (chain_id, expiry) = &*entry;
            if now < *expiry {
                if self
                    .chain_set
                    .chains
                    .get(chain_id)
                    .map(|h| h.is_usable())
                    .unwrap_or(false)
                {
                    return vec![chain_id.clone()];
                }
            }
        }

        let choices = self.pick_latency_weighted(1);
        if let Some(primary) = choices.first() {
            let expiry = now + std::time::Duration::from_secs(ttl_secs);
            self.sticky_map.insert(key, (primary.clone(), expiry));
        }
        choices
    }

    fn pick_region(
        &self,
        target: &TargetKey,
        target_geo: &HashMap<GeoTag, Vec<ChainId>>,
        fallback: &RoutingPolicy,
    ) -> Vec<ChainId> {
        let geo = resolve_geo(&target.host);
        if let Some(geo_tag) = geo {
            if let Some(preferred) = target_geo.get(&geo_tag) {
                let active: Vec<ChainId> = preferred
                    .iter()
                    .filter(|id| {
                        self.chain_set
                            .chains
                            .get(*id)
                            .map(|h| h.is_usable())
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect();
                if !active.is_empty() {
                    return active;
                }
            }
        }
        let inner = RouterImpl {
            chain_set: Arc::clone(&self.chain_set),
            policy: fallback.clone(),
            rr_cursor: AtomicU64::new(self.rr_cursor.load(Ordering::Relaxed)),
            sticky_map: DashMap::new(),
        };
        inner.dispatch(
            target,
            &FlowKey::TunFlow {
                five_tuple_hash: 0,
            },
        )
    }
}

/// Placeholder — Wave 3 will integrate maxmind GeoLite2. Always `None` for now.
fn resolve_geo(_host: &str) -> Option<GeoTag> {
    None
}

// ── Eviction task ─────────────────────────────────────────────────────────────

/// Spawned once per `RouterImpl` (when StickyPerFlow). Evicts expired entries.
pub async fn sticky_eviction_task(router: std::sync::Weak<RouterImpl>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let Some(r) = router.upgrade() else {
            return;
        };
        let now = Instant::now();
        r.sticky_map.retain(|_k, v| now < v.1);
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_set::ChainSet;
    use crate::chain_set::types::HealthProbeConfig;

    fn make_chain_set(ids: &[&str]) -> Arc<ChainSet> {
        use arc_swap::ArcSwap;
        use mirage_core::peer_table::PeerTable;
        use tokio::sync::Notify;

        let cs = ChainSet {
            chains: Arc::new(DashMap::new()),
            prewarm: Default::default(),
            peer_table: Arc::new(ArcSwap::from_pointee(PeerTable::empty())),
            peer_table_notify: Arc::new(Notify::new()),
            mesh_id: b"test".to_vec(),
            probe_config: HealthProbeConfig::default(),
        };
        // Mark all chains Active without real pools (not testing pool internals here).
        for id in ids {
            // We can't create a real TunnelPool without a running Tokio runtime
            // and actual connection, so we test at the router level using
            // pre-populated metrics only (pool field not accessed in routing logic).
            // This test skips handle insertion to avoid the dependency on real pools.
            let _ = id; // acknowledged — no handle inserted in unit tests
        }
        Arc::new(cs)
    }

    #[test]
    fn bypass_matcher_matches_cidr() {
        let matcher = BypassMatcher {
            cidrs_v4: vec!["10.0.0.0/8".parse().unwrap()],
            cidrs_v6: vec![],
            domains: vec![".local".to_string()],
            exact_ips: vec![],
        };
        let target_in = TargetKey {
            host: "10.1.2.3".to_string(),
            port: 80,
            protocol_hint: ProtocolHint::Tcp,
        };
        let target_out = TargetKey {
            host: "8.8.8.8".to_string(),
            port: 80,
            protocol_hint: ProtocolHint::Tcp,
        };
        let target_domain = TargetKey {
            host: "myhost.local".to_string(),
            port: 80,
            protocol_hint: ProtocolHint::Tcp,
        };
        assert!(matcher.matches(&target_in));
        assert!(!matcher.matches(&target_out));
        assert!(matcher.matches(&target_domain));
    }

    #[test]
    fn bypass_matcher_empty_never_matches() {
        let m = BypassMatcher::empty();
        let t = TargetKey {
            host: "1.2.3.4".to_string(),
            port: 80,
            protocol_hint: ProtocolHint::Unknown,
        };
        assert!(!m.matches(&t));
    }

    #[test]
    fn flow_key_stable_hash_consistent() {
        use std::net::SocketAddr;
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let fk1 = FlowKey::Socks5 {
            client_addr: addr,
            request_id: 42,
        };
        let fk2 = FlowKey::Socks5 {
            client_addr: addr,
            request_id: 42,
        };
        assert_eq!(fk1.stable_hash(), fk2.stable_hash());
    }

    #[test]
    fn round_robin_empty_set_returns_empty() {
        let cs = make_chain_set(&[]);
        let router = RouterImpl::new(Arc::clone(&cs), RoutingPolicy::RoundRobin);
        let t = TargetKey {
            host: "x.com".into(),
            port: 443,
            protocol_hint: ProtocolHint::Tls,
        };
        let f = FlowKey::TunFlow {
            five_tuple_hash: 1,
        };
        assert!(router.pick(&t, &f).is_empty());
    }
}
