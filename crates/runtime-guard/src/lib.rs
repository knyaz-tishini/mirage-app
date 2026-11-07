//! mirage-runtime-guard — per-process RSS memory pressure monitor.
//!
//! Samples the current process RSS at a configurable interval and compares it
//! against the system's total physical RAM.  On threshold crossings a
//! [`PressureLevel`] is published via a [`tokio::sync::watch`] channel so
//! callers can react (health endpoints, metrics, etc.).  When the fatal
//! threshold is first crossed the caller-supplied `on_fatal` closure is
//! invoked exactly once; the guard itself does **not** call
//! `std::process::exit` — that decision belongs to the caller.
//!
//! # Sysinfo on Windows
//!
//! `sysinfo` reads RSS on Windows via `GetProcessMemoryInfo` (PSAPI).  No
//! elevated privileges are required for querying the **current** process.
//! Admin rights are only needed when querying **other** processes.

use std::time::Duration;

use sysinfo::{Pid, System};
use tokio::sync::watch;
use tracing::{debug, error, warn};

// ── Public types ──────────────────────────────────────────────────────────────

/// Configuration for [`MemoryGuard`].
pub struct MemoryGuardConfig {
    /// How often to sample RSS.  Default: 10 s.
    pub sample_interval: Duration,
    /// Emit [`PressureLevel::Warn`] above this fraction of system RAM.
    /// Default: 0.70 (70 %).
    pub warn_threshold_pct: f32,
    /// Emit [`PressureLevel::Critical`] above this fraction of system RAM.
    /// Default: 0.85 (85 %).
    pub critical_threshold_pct: f32,
    /// Emit [`PressureLevel::Fatal`] and invoke `on_fatal` above this fraction.
    /// Default: 0.95 (95 %).
    pub fatal_threshold_pct: f32,
}

impl Default for MemoryGuardConfig {
    fn default() -> Self {
        Self {
            sample_interval: Duration::from_secs(10),
            warn_threshold_pct: 0.70,
            critical_threshold_pct: 0.85,
            fatal_threshold_pct: 0.95,
        }
    }
}

/// Memory pressure level published by [`MemoryGuard`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressureLevel {
    Ok,
    Warn,
    Critical,
    Fatal,
}

/// Handle to the background memory-guard task.
///
/// Dropping this handle does **not** stop the background task; the task runs
/// for the lifetime of the process.  The watch receiver can be cloned freely.
pub struct MemoryGuard {
    /// Subscribe to pressure level changes.
    pub level_rx: watch::Receiver<PressureLevel>,
}

impl MemoryGuard {
    /// Spawn the guard background task.
    ///
    /// `on_fatal` is called exactly once when the fatal threshold is first
    /// crossed.  The callback runs inside the guard task on the Tokio runtime;
    /// keep it short (e.g. log + `std::process::exit`).
    pub fn spawn(config: MemoryGuardConfig, on_fatal: Box<dyn FnOnce() + Send + 'static>) -> Self {
        let (level_tx, level_rx) = watch::channel(PressureLevel::Ok);

        tokio::spawn(guard_task(config, level_tx, on_fatal));

        Self { level_rx }
    }
}

// ── Background task ───────────────────────────────────────────────────────────

async fn guard_task(
    config: MemoryGuardConfig,
    level_tx: watch::Sender<PressureLevel>,
    on_fatal: Box<dyn FnOnce() + Send + 'static>,
) {
    let pid = Pid::from_u32(std::process::id());

    // Wrap on_fatal in an Option so we can call it at most once.
    let mut on_fatal = Some(on_fatal);

    let mut current_level = PressureLevel::Ok;
    let mut interval = tokio::time::interval(config.sample_interval);

    loop {
        interval.tick().await;

        // Refresh only the current process to keep overhead minimal.
        let mut sys = System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);

        let total_ram = sys.total_memory();
        if total_ram == 0 {
            // sysinfo not available on this platform/config — skip.
            debug!("runtime-guard: total_memory=0, skipping sample");
            continue;
        }

        let rss = match sys.process(pid) {
            Some(p) => p.memory(),
            None => {
                error!("runtime-guard: could not find own process in sysinfo");
                continue;
            }
        };

        let pct = rss as f32 / total_ram as f32;

        let new_level = if pct >= config.fatal_threshold_pct {
            PressureLevel::Fatal
        } else if pct >= config.critical_threshold_pct {
            PressureLevel::Critical
        } else if pct >= config.warn_threshold_pct {
            PressureLevel::Warn
        } else {
            PressureLevel::Ok
        };

        if new_level != current_level {
            log_transition(current_level, new_level, pct, rss, total_ram);
            current_level = new_level;
            // Ignore send error: if all receivers dropped the process is exiting.
            let _ = level_tx.send(new_level);
        } else {
            debug!(
                rss_mb = rss / 1024 / 1024,
                pct = format!("{:.1}%", pct * 100.0),
                level = ?new_level,
                "runtime-guard: sample"
            );
        }

        if new_level == PressureLevel::Fatal {
            if let Some(f) = on_fatal.take() {
                warn!("runtime-guard: fatal threshold crossed — invoking on_fatal");
                f();
            }
            // After firing, keep sampling so the watch channel stays accurate,
            // but on_fatal will not fire again.
        }
    }
}

fn log_transition(from: PressureLevel, to: PressureLevel, pct: f32, rss: u64, total: u64) {
    let rss_mb = rss / 1024 / 1024;
    let total_mb = total / 1024 / 1024;
    let pct_str = format!("{:.1}%", pct * 100.0);

    match to {
        PressureLevel::Ok => {
            tracing::info!(
                from = ?from, to = ?to,
                rss_mb, total_mb, pct = %pct_str,
                "runtime-guard: pressure relieved"
            );
        }
        PressureLevel::Warn => {
            warn!(
                from = ?from, to = ?to,
                rss_mb, total_mb, pct = %pct_str,
                "runtime-guard: WARN memory pressure"
            );
        }
        PressureLevel::Critical => {
            warn!(
                from = ?from, to = ?to,
                rss_mb, total_mb, pct = %pct_str,
                "runtime-guard: CRITICAL memory pressure"
            );
        }
        PressureLevel::Fatal => {
            tracing::error!(
                from = ?from, to = ?to,
                rss_mb, total_mb, pct = %pct_str,
                "runtime-guard: FATAL memory pressure — on_fatal will be called"
            );
        }
    }
}
