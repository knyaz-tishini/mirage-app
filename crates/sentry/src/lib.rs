//! mirage-sentry — always-on companion to `mirage-slavyanka`.
//!
//! Two concurrent jobs:
//!
//! 1. **Process supervisor** — spawns `slavyanka_binary` as a managed child.
//!    On unexpected exit, sleep with exponential backoff and respawn. Sentry
//!    tracks `running | crashed | unknown` for the heartbeat.
//!
//! 2. **Heartbeat poster** — every `heartbeat_interval_secs` POSTs JSON to
//!    `<monitor_url>/relay/clients/{client_id}/heartbeat` with
//!    `Authorization: Bearer <monitor_api_key>`. Body matches the monitor's
//!    `ClientHeartbeat` schema (`active_chains/total_bytes_tx/_rx/uptime_secs`)
//!    — slavyanka_status is encoded as the synthetic chain
//!    `slavyanka:<status>` in `active_chains[0].chain_name`. This avoids any
//!    monitor-side schema change for the MVP.
//!
//! On first boot sentry POSTs `/relay/clients` (no auth required) to obtain
//! a `client_id`; it persists the id to `<state_dir>/sentry-client-id.txt`
//! and reuses it across restarts.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct SentryConfig {
    /// Base URL of the monitor we report to (e.g. `https://lm-nl-1.zen-geldmaschine.net:19443`).
    pub monitor_url: String,
    /// Client-role bearer key.
    pub monitor_api_key: String,
    /// Heartbeat cadence in seconds (default 30).
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_interval_secs: u64,
    /// Absolute path to the slavyanka binary.
    pub slavyanka_binary: PathBuf,
    /// Absolute path to slavyanka's TOML config (passed via `--config`).
    pub slavyanka_config: PathBuf,
    /// Where to persist `sentry-client-id.txt`. Default
    /// `C:/ProgramData/mirage/state` on Windows.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    /// Initial restart backoff in seconds (default 1).
    #[serde(default = "default_initial_backoff_secs")]
    pub initial_backoff_secs: u64,
    /// Restart backoff cap in seconds (default 60).
    #[serde(default = "default_max_backoff_secs")]
    pub max_backoff_secs: u64,
}

fn default_heartbeat_secs() -> u64 { 30 }
fn default_initial_backoff_secs() -> u64 { 1 }
fn default_max_backoff_secs() -> u64 { 60 }

impl SentryConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read sentry config {:?}", path))?;
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("parse sentry config {:?}", path))?;
        Ok(cfg)
    }

    fn state_dir(&self) -> PathBuf {
        self.state_dir.clone().unwrap_or_else(|| {
            if cfg!(windows) {
                PathBuf::from("C:/ProgramData/mirage/state")
            } else {
                PathBuf::from("/var/lib/mirage")
            }
        })
    }
}

// ── Slavyanka status (shared state) ───────────────────────────────────────────

/// Atomic encoding of `Running=1 | Crashed=2 | Unknown=0`.
#[derive(Clone)]
pub struct SlavyankaStatus(Arc<AtomicU8>);

impl SlavyankaStatus {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }
    pub fn set_running(&self)  { self.0.store(1, Ordering::Relaxed); }
    pub fn set_crashed(&self)  { self.0.store(2, Ordering::Relaxed); }
    pub fn as_str(&self) -> &'static str {
        match self.0.load(Ordering::Relaxed) {
            1 => "running",
            2 => "crashed",
            _ => "unknown",
        }
    }
}

impl Default for SlavyankaStatus {
    fn default() -> Self { Self::new() }
}

// ── Wire-format types matching monitor's ClientRegistration/ClientHeartbeat ───

#[derive(Debug, Clone, Serialize)]
struct ClientRegistration {
    os: String,
    version: String,
    supported_transports: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ClientRegistrationResponse {
    client_id: String,
    #[allow(dead_code)]
    monitor_poll_interval_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ClientHeartbeat {
    active_chains: Vec<ClientChainStatus>,
    total_bytes_tx: u64,
    total_bytes_rx: u64,
    uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ClientChainStatus {
    chain_name: String,
    transport: String,
    latency_ms: Option<u64>,
    bytes_tx: u64,
    bytes_rx: u64,
    errors: u64,
    connections: u64,
}

// ── Client-id persistence ─────────────────────────────────────────────────────

/// Read `<state_dir>/sentry-client-id.txt`, returning `None` if absent/empty.
fn read_client_id(state_dir: &Path) -> Option<String> {
    let p = state_dir.join("sentry-client-id.txt");
    let s = std::fs::read_to_string(&p).ok()?;
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

fn write_client_id(state_dir: &Path, id: &str) -> Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("mkdir state_dir {:?}", state_dir))?;
    let p = state_dir.join("sentry-client-id.txt");
    let tmp = state_dir.join("sentry-client-id.txt.tmp");
    std::fs::write(&tmp, id.as_bytes())
        .with_context(|| format!("write tmp {:?}", tmp))?;
    std::fs::rename(&tmp, &p)
        .with_context(|| format!("rename {:?} -> {:?}", tmp, p))?;
    Ok(())
}

/// Either load a cached `client_id` or register fresh.
async fn ensure_client_id(client: &reqwest::Client, cfg: &SentryConfig) -> Result<String> {
    let state_dir = cfg.state_dir();
    if let Some(id) = read_client_id(&state_dir) {
        info!(client_id = %id, "[sentry] reusing persisted client_id");
        return Ok(id);
    }
    info!("[sentry] no cached client_id — registering with monitor");
    let body = ClientRegistration {
        os: std::env::consts::OS.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        supported_transports: vec!["slavyanka-reverse-ssh".to_string()],
    };
    let url = format!("{}/relay/clients", cfg.monitor_url.trim_end_matches('/'));
    let resp: ClientRegistrationResponse = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {}", url))?
        .error_for_status()
        .with_context(|| format!("monitor rejected registration at {}", url))?
        .json()
        .await
        .context("parse ClientRegistrationResponse")?;
    write_client_id(&state_dir, &resp.client_id)?;
    info!(client_id = %resp.client_id, "[sentry] registered, id persisted");
    Ok(resp.client_id)
}

// ── Heartbeat loop ────────────────────────────────────────────────────────────

async fn heartbeat_loop(
    client: reqwest::Client,
    cfg: SentryConfig,
    client_id: String,
    status: SlavyankaStatus,
    started_at: Instant,
) {
    let url = format!(
        "{}/relay/clients/{}/heartbeat",
        cfg.monitor_url.trim_end_matches('/'),
        client_id
    );
    let interval = Duration::from_secs(cfg.heartbeat_interval_secs.max(1));
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    loop {
        let body = ClientHeartbeat {
            active_chains: vec![ClientChainStatus {
                chain_name: format!("slavyanka:{}", status.as_str()),
                transport: "reverse-ssh".to_string(),
                latency_ms: None,
                bytes_tx: 0,
                bytes_rx: 0,
                errors: 0,
                connections: if status.as_str() == "running" { 1 } else { 0 },
            }],
            total_bytes_tx: 0,
            total_bytes_rx: 0,
            uptime_secs: started_at.elapsed().as_secs(),
        };
        let send = client
            .post(&url)
            .bearer_auth(&cfg.monitor_api_key)
            .json(&body)
            .send();

        tokio::select! {
            res = send => match res {
                Ok(r) if r.status().is_success() => {
                    debug!(status = ?status.as_str(), "[sentry] heartbeat ok");
                }
                Ok(r) => {
                    warn!(http = ?r.status(), "[sentry] heartbeat non-2xx");
                }
                Err(e) => {
                    warn!(error = %e, "[sentry] heartbeat HTTP error");
                }
            },
            _ = &mut shutdown => {
                info!("[sentry] heartbeat loop: shutdown");
                return;
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = &mut shutdown => {
                info!("[sentry] heartbeat loop: shutdown during sleep");
                return;
            }
        }
    }
}

// ── Slavyanka child supervisor ────────────────────────────────────────────────

async fn supervisor_loop(cfg: SentryConfig, status: SlavyankaStatus) -> Result<()> {
    let initial = Duration::from_secs(cfg.initial_backoff_secs.max(1));
    let cap = Duration::from_secs(cfg.max_backoff_secs.max(cfg.initial_backoff_secs).max(1));
    let mut delay = initial;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    // Track the current child so Ctrl-C can kill it promptly.
    let current: Arc<Mutex<Option<tokio::process::Child>>> = Arc::new(Mutex::new(None));

    loop {
        info!(
            binary = ?cfg.slavyanka_binary,
            config = ?cfg.slavyanka_config,
            "[sentry] spawning slavyanka"
        );
        let spawn_result = Command::new(&cfg.slavyanka_binary)
            .arg("--config")
            .arg(&cfg.slavyanka_config)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn();

        let child = match spawn_result {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "[sentry] spawn failed");
                status.set_crashed();
                // Wait then retry.
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = &mut shutdown => {
                        info!("[sentry] shutdown during spawn-error backoff");
                        return Ok(());
                    }
                }
                delay = (delay * 2).min(cap);
                continue;
            }
        };

        let pid = child.id();
        info!(pid = ?pid, "[sentry] slavyanka up");
        status.set_running();
        {
            let mut g = current.lock().await;
            *g = Some(child);
        }

        // Wait for the child or shutdown — whichever first.
        let wait_outcome = {
            let current = Arc::clone(&current);
            async move {
                let mut g = current.lock().await;
                if let Some(c) = g.as_mut() {
                    c.wait().await
                } else {
                    Err(std::io::Error::other("child slot empty"))
                }
            }
        };

        tokio::select! {
            res = wait_outcome => {
                status.set_crashed();
                match res {
                    Ok(st) => warn!(status = ?st, "[sentry] slavyanka exited"),
                    Err(e) => error!(error = %e, "[sentry] error waiting on slavyanka"),
                }
                // Drop the now-finished child slot before backoff.
                let mut g = current.lock().await;
                *g = None;
            }
            _ = &mut shutdown => {
                info!("[sentry] shutdown — killing slavyanka child");
                let mut g = current.lock().await;
                if let Some(mut c) = g.take() {
                    let _ = c.start_kill();
                    let _ = tokio::time::timeout(Duration::from_secs(5), c.wait()).await;
                }
                return Ok(());
            }
        }

        // Reset backoff if child lived long enough to be considered "stable".
        if delay > initial && status.as_str() == "crashed" {
            // No state about runtime — pessimistic, keep backing off.
        }

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = &mut shutdown => {
                info!("[sentry] shutdown during respawn backoff");
                return Ok(());
            }
        }
        delay = (delay * 2).min(cap);
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub async fn run(cfg: SentryConfig) -> Result<()> {
    if !cfg.slavyanka_binary.exists() {
        return Err(anyhow!(
            "slavyanka_binary {:?} does not exist",
            cfg.slavyanka_binary
        ));
    }
    if !cfg.slavyanka_config.exists() {
        return Err(anyhow!(
            "slavyanka_config {:?} does not exist",
            cfg.slavyanka_config
        ));
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;

    let client_id = ensure_client_id(&http, &cfg).await?;
    let status = SlavyankaStatus::new();
    let started_at = Instant::now();

    let hb_task = {
        let http = http.clone();
        let cfg = cfg.clone();
        let status = status.clone();
        tokio::spawn(async move {
            heartbeat_loop(http, cfg, client_id, status, started_at).await;
        })
    };

    let sup_res = supervisor_loop(cfg, status).await;
    hb_task.abort();
    sup_res
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let raw = r#"
            monitor_url = "https://lm-nl-1.zen-geldmaschine.net:19443"
            monitor_api_key = "client-key"
            slavyanka_binary = "C:/Program Files/Mirage/mirage-slavyanka.exe"
            slavyanka_config = "C:/ProgramData/mirage/slavyanka.toml"
        "#;
        let cfg: SentryConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.heartbeat_interval_secs, 30);
        assert!(cfg.state_dir.is_none());
        assert_eq!(cfg.initial_backoff_secs, 1);
        assert_eq!(cfg.max_backoff_secs, 60);
    }

    #[test]
    fn status_default_is_unknown() {
        let s = SlavyankaStatus::new();
        assert_eq!(s.as_str(), "unknown");
        s.set_running();
        assert_eq!(s.as_str(), "running");
        s.set_crashed();
        assert_eq!(s.as_str(), "crashed");
    }

    #[test]
    fn client_id_round_trips_through_state_dir() {
        let tmp = std::env::temp_dir().join(format!(
            "sentry-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(read_client_id(&tmp).is_none());
        write_client_id(&tmp, "client-abc").unwrap();
        assert_eq!(read_client_id(&tmp).as_deref(), Some("client-abc"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
