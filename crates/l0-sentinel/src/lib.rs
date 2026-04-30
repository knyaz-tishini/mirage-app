//! mirage-l0-sentinel library — embeddable sentinel logic.
//!
//! Standalone use: `main.rs` parses CLI, calls [`run`].
//! Embedded use (mobile orchestrator): `tokio::task::spawn(mirage_l0_sentinel::run(cfg))`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use mirage_client_core::monitor_link::MonitorLink;
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

pub mod crash_report;
pub mod status_server;
pub mod supervisor;

use crash_report::report_crash;
use status_server::{MonitorInfo, TunSpawnCfg};
use supervisor::{
    build_crash_report, cli_may_respawn, kill_tun, log_crash, record_cli_respawn, spawn_child,
    ChildState,
};

// ── Phase enum ────────────────────────────────────────────────────────────────

/// Current bootstrap phase of the sentinel process.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "phase")]
pub enum SentinelPhase {
    /// Process started, before any other work.
    Starting,
    /// MonitorLink spawned, waiting for first config.
    FetchingConfig { monitor_url: String },
    /// First config received and client-config file materialised.
    ConfigReady { version: u64, chains: usize, materialised_at_ms: u128 },
    /// Child process is running.
    ChildRunning { pid: Option<u32> },
    /// Fatal error; run() returned.
    Error { message: String },
    /// monitor_url + api_key not yet provided.
    WaitingOnboard,
    /// Monitor unreachable, using cached config.
    ConfigStale { since_ms: u64 },
}

// ── Public config ─────────────────────────────────────────────────────────────

/// Configuration for the sentinel bootstrap process.
#[derive(Debug, Deserialize)]
pub struct SentinelConfig {
    /// Base URL of mirage2monitor (e.g. `"http://127.0.0.1:19410"`).
    pub monitor_url: String,
    /// Optional Bearer token for monitor API.
    #[serde(default)]
    pub monitor_api_key: Option<String>,
    /// Path where the materialised ClientConfig JSON will be written.
    pub client_config_path: PathBuf,
    /// Path to the mirage-client-cli binary.
    pub client_binary: PathBuf,
    /// Args to pass; `{client_config_path}` is substituted with the real path.
    #[serde(default = "default_client_args")]
    pub client_args: Vec<String>,
    /// Base config JSON (everything except `chains`).
    pub client_template: PathBuf,
    /// Seconds between monitor polls when no change detected.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// When `true`, refuse to overwrite an unmanaged existing config file.
    #[serde(default = "default_respect_user_config")]
    pub respect_user_config: bool,
    /// Install a panic hook that calls `exit(1)` on panic.
    #[serde(default = "default_install_panic_hook")]
    pub install_panic_hook: bool,
    /// Call `exit(0)` when the memory guard hits fatal threshold.
    #[serde(default = "default_exit_on_memory_fatal")]
    pub exit_on_memory_fatal: bool,
    /// Path to tun binary. Defaults to `<exe_dir>/mirage2tun[.exe]` when absent.
    #[serde(default)]
    pub tun_binary: Option<PathBuf>,
    /// Args for tun. Defaults to `["debug-run"]`.
    #[serde(default = "default_tun_args")]
    pub tun_args: Vec<String>,
}

pub(crate) fn default_client_args() -> Vec<String> {
    vec!["--config".to_owned(), "{client_config_path}".to_owned()]
}
pub(crate) fn default_poll_interval() -> u64 { 30 }
pub(crate) fn default_respect_user_config() -> bool { true }
pub(crate) fn default_install_panic_hook() -> bool { true }
pub(crate) fn default_exit_on_memory_fatal() -> bool { true }
pub(crate) fn default_tun_args() -> Vec<String> { vec!["debug-run".to_owned()] }

// ── Managed marker ────────────────────────────────────────────────────────────

pub(crate) const MANAGED_BY_VALUE: &str = "mirage-l0-sentinel";

pub(crate) fn is_managed(path: &PathBuf) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    if raw.trim_start().starts_with("// mirage-l0-sentinel: managed") {
        return true;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
        if v.get("_managed_by").and_then(|v| v.as_str()) == Some(MANAGED_BY_VALUE) {
            return true;
        }
    }
    false
}

// ── Config materialisation ────────────────────────────────────────────────────

pub(crate) fn materialise(
    template_raw: &str,
    chains: Vec<mirage_core::config::ChainConfig>,
) -> Result<String> {
    let mut obj: serde_json::Value = serde_json::from_str(template_raw)
        .context("failed to parse client-template.json")?;

    let map = obj
        .as_object_mut()
        .context("client-template.json must be a JSON object")?;

    map.remove("chains");

    let chains_val = serde_json::to_value(&chains).context("failed to serialise chains")?;
    map.insert("chains".to_owned(), chains_val);

    map.insert(
        "_managed_by".to_owned(),
        serde_json::Value::String(MANAGED_BY_VALUE.to_owned()),
    );

    serde_json::to_string_pretty(&obj).context("failed to serialise materialised config")
}

pub(crate) fn write_atomic(path: &PathBuf, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)
        .with_context(|| format!("write tmp file {:?}", tmp))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {:?} → {:?}", tmp, path))?;
    Ok(())
}

// ── Child process helpers ─────────────────────────────────────────────────────

pub(crate) fn build_child_args(template_args: &[String], config_path: &PathBuf) -> Vec<String> {
    let path_str = config_path.to_string_lossy();
    template_args
        .iter()
        .map(|a| a.replace("{client_config_path}", &path_str))
        .collect()
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run the sentinel bootstrap loop.
pub async fn run(cfg: SentinelConfig) -> Result<()> {
    // ── Shared supervisor state ───────────────────────────────────────────────
    let children = std::sync::Arc::new(Mutex::new(ChildState::new()));
    let monitor_info = std::sync::Arc::new(std::sync::Mutex::new(MonitorInfo {
        monitor_url: cfg.monitor_url.clone(),
        monitor_reachable: false,
    }));

    // ── Status server ─────────────────────────────────────────────────────────
    let (phase_tx, phase_rx) = status_server::phase_channel();
    let publisher = status_server::spawn(
        phase_rx,
        phase_tx,
        std::sync::Arc::clone(&children),
        TunSpawnCfg {
            tun_binary: cfg.tun_binary.clone(),
            tun_args: cfg.tun_args.clone(),
        },
        std::sync::Arc::clone(&monitor_info),
    );
    publisher.set(SentinelPhase::Starting);

    // ── Panic hook ────────────────────────────────────────────────────────────
    let want_hook = cfg.install_panic_hook
        || std::env::var("MIRAGE_SENTINEL_PANIC_EXIT").as_deref() == Ok("1");

    if want_hook {
        std::panic::set_hook(Box::new(|info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "<unknown>".to_string());
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            tracing::error!(location = %location, message = %msg, "panic — process exiting for respawn");
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(1);
        }));
    }

    info!(
        "[sentinel] monitor_url={} client_binary={:?}",
        cfg.monitor_url, cfg.client_binary
    );

    // ── Managed-file guard ────────────────────────────────────────────────────
    let config_exists = cfg.client_config_path.exists();
    let managed = config_exists && is_managed(&cfg.client_config_path);

    if config_exists && !managed && cfg.respect_user_config {
        warn!(
            "[sentinel] {:?} exists but has no managed marker — respect_user_config=true, skipping overwrite",
            cfg.client_config_path
        );
        warn!("[sentinel] spawning client with existing config; sentinel will not poll monitor");
        return run_client_passthrough(&cfg, children, &publisher, monitor_info).await;
    }

    // ── Load template ─────────────────────────────────────────────────────────
    let template_raw = std::fs::read_to_string(&cfg.client_template)
        .with_context(|| format!("cannot read client-template {:?}", cfg.client_template))?;
    {
        let v: serde_json::Value =
            serde_json::from_str(&template_raw).context("client-template.json is not valid JSON")?;
        if !v.is_object() {
            bail!("client-template.json must be a JSON object");
        }
    }

    // ── Spawn MonitorLink ─────────────────────────────────────────────────────
    let monitor_url = cfg.monitor_url.clone();
    let monitor_api_key = cfg.monitor_api_key.clone();

    info!("[sentinel] spawning MonitorLink → {}", monitor_url);
    publisher.set(SentinelPhase::FetchingConfig { monitor_url: monitor_url.clone() });
    let (link, mut config_rx) = MonitorLink::new(monitor_url.clone(), monitor_api_key);
    link.spawn();

    let timeout = Duration::from_secs(cfg.poll_interval_secs.max(10));
    info!("[sentinel] waiting for first monitor config (timeout {}s)…", timeout.as_secs());

    let first_cfg = match tokio::time::timeout(timeout, async {
        loop {
            config_rx.changed().await.ok();
            if config_rx.borrow().is_some() {
                return config_rx.borrow().clone().unwrap();
            }
        }
    })
    .await
    {
        Ok(cfg) => cfg,
        Err(e) => {
            let msg = format!("timed out waiting for first monitor config — is the monitor reachable? {e}");
            publisher.set(SentinelPhase::Error { message: msg.clone() });
            anyhow::bail!(msg);
        }
    };

    {
        let mut info = monitor_info.lock().unwrap_or_else(|e| e.into_inner());
        info.monitor_reachable = true;
    }

    info!(
        "[sentinel] first monitor config received: version={} chains={}",
        first_cfg.version,
        first_cfg.chains.len()
    );

    let chains: Vec<_> = first_cfg.chains.iter().map(|c| c.chain.clone()).collect();
    let full_json = match materialise(&template_raw, chains) {
        Ok(j) => j,
        Err(e) => {
            publisher.set(SentinelPhase::Error { message: e.to_string() });
            return Err(e);
        }
    };
    if let Err(e) = write_atomic(&cfg.client_config_path, &full_json)
        .context("failed to write initial client config")
    {
        publisher.set(SentinelPhase::Error { message: e.to_string() });
        return Err(e);
    }
    info!("[sentinel] wrote {:?}", cfg.client_config_path);

    let materialised_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    publisher.set(SentinelPhase::ConfigReady {
        version: first_cfg.version,
        chains: first_cfg.chains.len(),
        materialised_at_ms,
    });

    // ── Spawn cli ─────────────────────────────────────────────────────────────
    let child_args = build_child_args(&cfg.client_args, &cfg.client_config_path);
    info!(
        "[sentinel] spawning cli {:?} with args {:?}",
        cfg.client_binary, child_args
    );

    let cli_handle = match spawn_child(&cfg.client_binary, &child_args) {
        Ok(h) => h,
        Err(e) => {
            publisher.set(SentinelPhase::Error { message: e.to_string() });
            return Err(e);
        }
    };

    let cli_pid = cli_handle.process.id();
    {
        let mut ch = children.lock().await;
        ch.cli = Some(cli_handle);
    }
    publisher.set(SentinelPhase::ChildRunning { pid: cli_pid });

    // ── Memory guard ─────────────────────────────────────────────────────────
    let exit_on_fatal = cfg.exit_on_memory_fatal;
    let _memory_guard = MemoryGuard::spawn(
        MemoryGuardConfig::default(),
        Box::new(move || {
            warn!("[sentinel] memory guard: fatal threshold — exiting for respawn");
            if exit_on_fatal {
                std::process::exit(0);
            }
        }),
    );

    // ── Main supervisor loop ─────────────────────────────────────────────────
    loop {
        // Poll cli exit
        let cli_exited = {
            let mut ch = children.lock().await;
            if let Some(ref mut handle) = ch.cli {
                match handle.process.try_wait() {
                    Ok(Some(status)) => Some(status),
                    Ok(None) => None,
                    Err(e) => {
                        error!("[sentinel] cli try_wait error: {}", e);
                        None
                    }
                }
            } else {
                None
            }
        };

        if let Some(exit_status) = cli_exited {
            let report = {
                let ch = children.lock().await;
                if let Some(ref handle) = ch.cli {
                    Some(build_crash_report("cli", exit_status, handle).await)
                } else {
                    None
                }
            };

            if let Some(report) = report {
                log_crash(&report);
                report_crash(&report, Some(&monitor_url)).await;

                let exit_code = report.exit_code.unwrap_or(-1);
                publisher.set(SentinelPhase::Error {
                    message: format!("cli crashed: {:?} (code {})", report.kind, exit_code),
                });

                // Check respawn eligibility
                let may_respawn = {
                    let mut ch = children.lock().await;
                    ch.cli = None;
                    cli_may_respawn(&mut ch)
                };

                if may_respawn {
                    info!("[sentinel] respawning cli…");
                    match spawn_child(&cfg.client_binary, &child_args) {
                        Ok(new_handle) => {
                            let new_pid = new_handle.process.id();
                            let mut ch = children.lock().await;
                            record_cli_respawn(&mut ch);
                            ch.cli = Some(new_handle);
                            publisher.set(SentinelPhase::ChildRunning { pid: new_pid });
                            info!("[sentinel] cli respawned pid={:?}", new_pid);
                        }
                        Err(e) => {
                            error!("[sentinel] cli respawn failed: {}", e);
                            publisher.set(SentinelPhase::Error {
                                message: format!("cli respawn failed: {}", e),
                            });
                            // Fatal — exit so orchestrator/OS can respawn sentinel
                            std::process::exit(1);
                        }
                    }
                } else {
                    error!("[sentinel] cli exceeded respawn limit — exiting");
                    std::process::exit(1);
                }
            }
        }

        // Monitor config updates
        if config_rx.has_changed().unwrap_or(false) {
            config_rx.mark_unchanged();
            let snapshot = config_rx.borrow().clone();
            if let Some(new_cfg) = snapshot {
                info!(
                    "[sentinel] monitor config updated: version={} chains={} — rewriting {:?}",
                    new_cfg.version,
                    new_cfg.chains.len(),
                    cfg.client_config_path
                );
                let chains: Vec<_> = new_cfg.chains.iter().map(|c| c.chain.clone()).collect();
                match materialise(&template_raw, chains) {
                    Ok(json) => match write_atomic(&cfg.client_config_path, &json) {
                        Ok(()) => {
                            info!("[sentinel] config rewritten — restart client manually to apply (v1)");
                        }
                        Err(e) => error!("[sentinel] failed to rewrite config: {}", e),
                    },
                    Err(e) => error!("[sentinel] materialise error: {}", e),
                }
            }
        }

        // Ctrl-C
        let ctrl_c_hit = tokio::select! {
            _ = tokio::signal::ctrl_c() => true,
            _ = tokio::time::sleep(Duration::from_millis(500)) => false,
        };

        if ctrl_c_hit {
            info!("[sentinel] Ctrl-C — shutting down children");
            let mut ch = children.lock().await;
            if let Some(ref mut handle) = ch.tun {
                kill_tun(handle).await;
            }
            ch.tun = None;
            if let Some(ref mut handle) = ch.cli {
                let _ = handle.process.kill().await;
                match tokio::time::timeout(Duration::from_secs(5), handle.process.wait()).await {
                    Ok(Ok(s)) => info!("[sentinel] cli exited {:?}", s),
                    Ok(Err(e)) => warn!("[sentinel] cli wait: {}", e),
                    Err(_) => warn!("[sentinel] cli did not exit within 5s"),
                }
            }
            ch.cli = None;
            std::process::exit(0);
        }
    }
}

/// Passthrough mode: client file is unmanaged. Spawn client as-is, wait for exit.
async fn run_client_passthrough(
    cfg: &SentinelConfig,
    children: std::sync::Arc<Mutex<ChildState>>,
    publisher: &status_server::PhasePublisher,
    _monitor_info: std::sync::Arc<std::sync::Mutex<MonitorInfo>>,
) -> Result<()> {
    let child_args = build_child_args(&cfg.client_args, &cfg.client_config_path);
    info!(
        "[sentinel][passthrough] spawning {:?} with args {:?}",
        cfg.client_binary, child_args
    );

    let handle = spawn_child(&cfg.client_binary, &child_args)
        .with_context(|| format!("failed to spawn {:?}", cfg.client_binary))?;

    let pid = handle.process.id();
    {
        let mut ch = children.lock().await;
        ch.cli = Some(handle);
    }
    publisher.set(SentinelPhase::ChildRunning { pid });

    loop {
        let exited = {
            let mut ch = children.lock().await;
            if let Some(ref mut h) = ch.cli {
                h.process.try_wait().ok().flatten()
            } else {
                break;
            }
        };

        if let Some(status) = exited {
            let code = status.code().unwrap_or(-1);
            info!("[sentinel][passthrough] client exited with code {}", code);
            std::process::exit(code);
        }

        let ctrl_c_hit = tokio::select! {
            _ = tokio::signal::ctrl_c() => true,
            _ = tokio::time::sleep(Duration::from_millis(500)) => false,
        };

        if ctrl_c_hit {
            info!("[sentinel][passthrough] Ctrl-C — terminating client");
            let mut ch = children.lock().await;
            if let Some(ref mut h) = ch.cli {
                let _ = h.process.kill().await;
                let _ = tokio::time::timeout(Duration::from_secs(5), h.process.wait()).await;
            }
            std::process::exit(0);
        }
    }

    Ok(())
}
