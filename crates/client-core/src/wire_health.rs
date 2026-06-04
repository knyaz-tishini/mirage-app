//! Per-wire liveness map for the SOCKS5 data path.
//!
//! Mirrors [`mirage_tun_core::wire_health::WireHealthMap`] semantics but lives
//! inside `mirage-client-core` so the SOCKS5 path can enforce instant
//! fail-closed without a cross-crate dependency on `mirage-tun-core`.
//!
//! ## Design
//!
//! Keyed by `chain_id` (String). Thread-safe via `std::sync::RwLock` held only
//! for the duration of a single map lookup — **never** across `.await` points
//! (invariant 1 of using `std::sync` in async code).
//!
//! ## Thresholds
//!
//! - 2 consecutive failures → wire marked unhealthy (instant fail-closed on next flow)
//! - Auto-recover probe after 30 s (one attempt allowed — if it succeeds, healthy again)

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const FAIL_STREAK_THRESHOLD: u32 = 2;
const RECOVER_AFTER_SECS: u64 = 30;

/// Per-wire liveness map. Shared via `Arc`; clone-cheap.
pub struct WireHealthMap {
    inner: RwLock<HashMap<String, WireState>>,
}

#[derive(Clone)]
struct WireState {
    healthy: bool,
    fail_streak: u32,
    last_changed: Instant,
}

impl WireHealthMap {
    /// Construct a new, empty map wrapped in `Arc`.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(HashMap::new()),
        })
    }

    /// Record a successful pool lease for `wire` (chain_id).
    ///
    /// Resets `fail_streak` and marks the wire healthy.
    pub fn record_success(&self, wire: &str) {
        let mut map = match self.inner.write() {
            Ok(m) => m,
            Err(_) => return, // poisoned lock — skip, do not block the data path
        };
        let state = map.entry(wire.to_owned()).or_insert_with(|| WireState {
            healthy: true,
            fail_streak: 0,
            last_changed: Instant::now(),
        });
        let was_degraded = !state.healthy || state.fail_streak > 0;
        state.fail_streak = 0;
        state.healthy = true;
        if was_degraded {
            state.last_changed = Instant::now();
        }
    }

    /// Record a failed pool lease or target-send for `wire` (chain_id).
    ///
    /// Marks the wire unhealthy after `FAIL_STREAK_THRESHOLD` consecutive
    /// failures so subsequent flows skip it immediately instead of waiting for
    /// pool-lease timeout.
    pub fn record_failure(&self, wire: &str) {
        let mut map = match self.inner.write() {
            Ok(m) => m,
            Err(_) => return,
        };
        let state = map.entry(wire.to_owned()).or_insert_with(|| WireState {
            healthy: true,
            fail_streak: 0,
            last_changed: Instant::now(),
        });
        state.fail_streak += 1;
        if state.fail_streak >= FAIL_STREAK_THRESHOLD && state.healthy {
            state.healthy = false;
            state.last_changed = Instant::now();
        }
    }

    /// Fast health check.  Absent entry → `true` (assume healthy on first use).
    /// Poisoned lock → `true` (fail-open for liveness — better to try than to
    /// permanently black-hole a wire).
    ///
    /// Auto-recover: an unhealthy wire that has been down for longer than
    /// `RECOVER_AFTER_SECS` returns `true` (allow one probe attempt). The
    /// subsequent `record_success` or `record_failure` updates state.
    #[inline]
    pub fn is_healthy(&self, wire: &str) -> bool {
        let map = match self.inner.read() {
            Ok(m) => m,
            Err(_) => return true,
        };
        match map.get(wire) {
            None => true,
            Some(s) if s.healthy => true,
            Some(s) => {
                // Unhealthy but past cooldown → allow one probe attempt.
                s.last_changed.elapsed() >= Duration::from_secs(RECOVER_AFTER_SECS)
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_wire_is_healthy() {
        let m = WireHealthMap::new();
        assert!(m.is_healthy("chain-a"));
    }

    #[test]
    fn two_failures_mark_unhealthy() {
        let m = WireHealthMap::new();
        m.record_failure("chain-a");
        assert!(m.is_healthy("chain-a"), "one failure must not mark unhealthy");
        m.record_failure("chain-a");
        assert!(!m.is_healthy("chain-a"), "two failures must mark unhealthy");
    }

    #[test]
    fn success_after_failures_restores_health() {
        let m = WireHealthMap::new();
        m.record_failure("chain-a");
        m.record_failure("chain-a");
        assert!(!m.is_healthy("chain-a"));
        m.record_success("chain-a");
        assert!(m.is_healthy("chain-a"));
    }

    #[test]
    fn cooldown_allows_probe_after_recover_period() {
        let m = WireHealthMap::new();
        m.record_failure("chain-a");
        m.record_failure("chain-a");
        assert!(!m.is_healthy("chain-a"));
        {
            let mut map = m.inner.write().unwrap();
            let s = map.get_mut("chain-a").unwrap();
            s.last_changed = Instant::now() - Duration::from_secs(RECOVER_AFTER_SECS + 1);
        }
        assert!(m.is_healthy("chain-a"), "cooldown must allow probe attempt");
    }
}
