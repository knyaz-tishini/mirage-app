//! mirage-l0-sentinel library — embeddable sentinel logic.
//!
//! Standalone use: `main.rs` parses CLI, calls [`run`].
//! Embedded use (mobile orchestrator): `tokio::task::spawn(mirage_l0_sentinel::run(cfg))`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use mirage_client_core::monitor_link::MonitorLink;
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use serde::Deserialize;
use tracing::{error, info, warn};

// ── Public config ─────────────────────────────────────────────────────────────

/// Configuration for the sentinel bootstrap process.
///
/// Construct this from a TOML file or programmatically for embedded use.
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
    ///
    /// Set `false` for embedded use (mobile orchestrator owns the process).
    /// Also honoured when env var `MIRAGE_SENTINEL_PANIC_EXIT=1` is set.
    #[serde(default = "default_install_panic_hook")]
    pub install_panic_hook: bool,
    /// Call `exit(0)` when the memory guard hits fatal threshold.
    ///
    /// Set `false` for embedded use; the callback will log but not exit.
    #[serde(default = "default_exit_on_memory_fatal")]
    pub exit_on_memory_fatal: bool,
}

pub(crate) fn default_client_args() -> Vec<String> {
    vec!["--config".to_owned(), "{client_config_path}".to_owned()]
}
pub(crate) fn default_poll_interval() -> u64 { 30 }
pub(crate) fn default_respect_user_config() -> bool { true }
pub(crate) fn default_install_panic_hook() -> bool { true }
pub(crate) fn default_exit_on_memory_fatal() -> bool { true }

// ── Managed marker ────────────────────────────────────────────────────────────

pub(crate) const MANAGED_BY_VALUE: &str = "mirage-l0-sentinel";

/// Returns `true` if the existing file at `path` contains our managed marker.
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

/// Build a full ClientConfig JSON by merging template + chains from monitor.
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

/// Write `content` to `path` atomically (write to `.tmp`, then rename).
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
///
/// - Installs panic hook if `cfg.install_panic_hook` is `true` or env
///   `MIRAGE_SENTINEL_PANIC_EXIT=1` is set.
/// - Spawns [`MemoryGuard`]; calls `exit(0)` on fatal pressure only when
///   `cfg.exit_on_memory_fatal` is `true`.
/// - Does **not** initialise `tracing_subscriber` — caller owns that.
pub async fn run(cfg: SentinelConfig) -> Result<()> {
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
        return run_client_passthrough(&cfg).await;
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
    let (link, mut config_rx) = MonitorLink::new(monitor_url, monitor_api_key);
    link.spawn();

    let timeout = Duration::from_secs(cfg.poll_interval_secs.max(10));
    info!("[sentinel] waiting for first monitor config (timeout {}s)…", timeout.as_secs());

    let first_cfg = tokio::time::timeout(timeout, async {
        loop {
            config_rx.changed().await.ok();
            if config_rx.borrow().is_some() {
                return config_rx.borrow().clone().unwrap();
            }
        }
    })
    .await
    .context("timed out waiting for first monitor config — is the monitor reachable?")?;

    info!(
        "[sentinel] first monitor config received: version={} chains={}",
        first_cfg.version,
        first_cfg.chains.len()
    );

    let chains: Vec<_> = first_cfg.chains.iter().map(|c| c.chain.clone()).collect();
    let full_json = materialise(&template_raw, chains)?;
    write_atomic(&cfg.client_config_path, &full_json)
        .context("failed to write initial client config")?;
    info!("[sentinel] wrote {:?}", cfg.client_config_path);

    // ── Spawn client ──────────────────────────────────────────────────────────
    let child_args = build_child_args(&cfg.client_args, &cfg.client_config_path);
    info!(
        "[sentinel] spawning {:?} with args {:?}",
        cfg.client_binary, child_args
    );

    let mut child = tokio::process::Command::new(&cfg.client_binary)
        .args(&child_args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn {:?}", cfg.client_binary))?;

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

    // ── Config-watch loop + process supervisor ────────────────────────────────
    loop {
        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(s) => {
                        let code = s.code().unwrap_or(-1);
                        info!("[sentinel] client exited with code {}", code);
                        std::process::exit(code);
                    }
                    Err(e) => {
                        error!("[sentinel] wait() error: {}", e);
                        std::process::exit(1);
                    }
                }
            }

            changed = config_rx.changed() => {
                if changed.is_err() {
                    warn!("[sentinel] monitor watch channel closed");
                    break;
                }
                let snapshot = config_rx.borrow().clone();
                let Some(new_cfg) = snapshot else { continue };

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

            _ = tokio::signal::ctrl_c() => {
                info!("[sentinel] Ctrl-C received — terminating client");
                let _ = child.kill().await;
                match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                    Ok(Ok(s)) => info!("[sentinel] client exited with {:?}", s),
                    Ok(Err(e)) => warn!("[sentinel] wait after kill: {}", e),
                    Err(_) => warn!("[sentinel] client did not exit within 5s after kill"),
                }
                std::process::exit(0);
            }
        }
    }

    let status = child.wait().await.context("wait() failed")?;
    let code = status.code().unwrap_or(-1);
    info!("[sentinel] client exited with code {}", code);
    std::process::exit(code);
}

/// Passthrough mode: client file is unmanaged. Spawn client as-is, wait for exit.
async fn run_client_passthrough(cfg: &SentinelConfig) -> Result<()> {
    let child_args = build_child_args(&cfg.client_args, &cfg.client_config_path);
    info!(
        "[sentinel][passthrough] spawning {:?} with args {:?}",
        cfg.client_binary, child_args
    );

    let mut child = tokio::process::Command::new(&cfg.client_binary)
        .args(&child_args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn {:?}", cfg.client_binary))?;

    tokio::select! {
        status = child.wait() => {
            let code = status?.code().unwrap_or(-1);
            info!("[sentinel][passthrough] client exited with code {}", code);
            std::process::exit(code);
        }
        _ = tokio::signal::ctrl_c() => {
            info!("[sentinel][passthrough] Ctrl-C — terminating client");
            let _ = child.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            std::process::exit(0);
        }
    }
}
