//! mirage-l0-sentinel library — embeddable sentinel logic.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use mirage_client_core::{
    chain_set::ChainSet,
    monitor_link::{MonitorLink, MonitorTransport},
};
use mirage_core::mirage::mesh::PeerTable;
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};
use tracing::{error, info, warn};

pub mod chain_request;
pub mod crash_report;
pub mod debug_shell;
pub mod log_upload;
pub mod monitor_urls;
pub mod onboarding;
pub mod pid_file;
pub mod sentinel_id;
pub mod status_server;
pub mod supervisor;
pub mod telemetry;
pub mod tun_config;

use crash_report::{report_crash, spawn_queue_drain};
use status_server::{MonitorInfo, TunSpawnCfg, reboot_channel};
use supervisor::{
    adopt_child, build_crash_report, cli_may_respawn, kill_tun, log_crash, record_cli_respawn,
    resolve_supervisor_binary, spawn_child, spawn_child_with_flags, ChildState,
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW,
};

// ── Phase enum ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "phase")]
pub enum SentinelPhase {
    Starting,
    FetchingConfig { monitor_url: String },
    ConfigReady { version: u64, chains: usize, materialised_at_ms: u128 },
    ChildRunning { pid: Option<u32> },
    Error { message: String },
    WaitingOnboard,
    ConfigStale { since_ms: u64 },
}

// ── Public config ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SentinelConfig {
    /// Preferred: list of monitor base URLs to race on startup.
    #[serde(default)]
    pub monitor_urls: Vec<String>,
    /// Deprecated: single URL. Used only when `monitor_urls` is empty.
    #[serde(default)]
    pub monitor_url: String,
    #[serde(default)]
    pub monitor_api_key: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    pub client_config_path: PathBuf,
    pub client_binary: PathBuf,
    #[serde(default = "default_client_args")]
    pub client_args: Vec<String>,
    pub client_template: PathBuf,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_respect_user_config")]
    pub respect_user_config: bool,
    #[serde(default = "default_install_panic_hook")]
    pub install_panic_hook: bool,
    #[serde(default = "default_exit_on_memory_fatal")]
    pub exit_on_memory_fatal: bool,
    #[serde(default)]
    pub tun_binary: Option<PathBuf>,
    /// Cross-platform default; used when no per-OS override matches.
    #[serde(default = "default_tun_args")]
    pub tun_args: Vec<String>,
    /// Windows-only override. Win mirage2tun is a service binary with subcommands
    /// (`debug-run`, `run`, ...) — `--config` style args are rejected.
    #[serde(default)]
    pub tun_args_windows: Option<Vec<String>>,
    /// macOS-only override. macOS mirage2tun takes `--config <path>`.
    #[serde(default)]
    pub tun_args_macos: Option<Vec<String>>,
    #[serde(default = "default_telemetry_interval")]
    pub telemetry_interval_secs: u64,
    /// How often (seconds) to upload cli/tun stderr tails to monitor.
    /// Set to 0 to disable. Default: 60.
    #[serde(default = "default_logs_upload_interval")]
    pub logs_upload_interval_secs: u64,
    /// Which cli variant to spawn:
    /// - `"cli1"` (default) — legacy single-process `mirage-client-cli`.
    /// - `"cli2"` — new split-arch `mirage-chain-supervisor` (supervisor +
    ///   workers per chain).  Required for soft-restart capability and
    ///   per-chain OTA.
    #[serde(default = "default_cli_variant")]
    pub cli_variant: String,
    /// Path to the chain-supervisor binary (cli2).  Defaults to
    /// `<exe_dir>/mirage-chain-supervisor.exe` on Windows (or without .exe on
    /// other platforms) when `cli_variant = "cli2"` and this field is `None`.
    #[serde(default)]
    pub supervisor_binary: Option<PathBuf>,
    /// Args passed to the supervisor.  The `{client_config_path}` token is
    /// substituted just like in `client_args`.
    ///
    /// Note: `mirage-chain-supervisor` reads its config as a positional
    /// argument (`argv[1]`), not via `--config`.  The default here is
    /// `["{client_config_path}"]` (bare path, no flag).
    #[serde(default = "default_supervisor_args")]
    pub supervisor_args: Vec<String>,
    /// Path where sentinel writes auto-generated tun.json (relay_addrs etc).
    /// If None, sentinel does not generate tun.json.
    #[serde(default)]
    pub tun_config_path: Option<PathBuf>,
    /// Template tun.json — copied into tun_config_path with relay_addrs filled in.
    /// If None, sentinel writes a minimal default tun.json stub.
    #[serde(default)]
    pub tun_template_path: Option<PathBuf>,
    /// Path to the `mirage-debug-shell` binary spawned on
    /// `WsServerFrame::DebugTunnelOpen`. When `None`, debug-shell features are
    /// silently disabled (incoming frames are logged + ignored).
    #[serde(default)]
    pub debug_shell_binary: Option<PathBuf>,
    /// Directory where sentinel persists transient state — `monitor_urls.json`
    /// (dynamic monitor URL list) and `debug-tunnel.json` (DebugTunnelOpen
    /// payload). Defaults to the directory of `client_config_path`.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,

    // ── Stage 3 identity (sent on register; nothing axis-related here) ───
    //
    // Sentinel is passive: after register it just subscribes to WS and
    // spawns cli. The monitor pushes chains on its own schedule. If the
    // operator (or a debug UI on the client) wants to request a specific
    // chain, they call /relay/clients/{id}/chains/request directly — that
    // is NOT sentinel's job.
    /// Mesh id sent on register + carried in handshakes. Defaults to
    /// `"v3-staging"` to match the current staging mesh.
    #[serde(default = "default_mesh_id")]
    pub mesh_id: String,
    /// Human label that surfaces in /relay/clients responses
    /// (`sentry-XXX`). Hardcoded per build profile or set in toml.
    #[serde(default)]
    pub shortname: Option<String>,
    /// `sentry-client | sentry-server-lm | sentry-server-node`.
    /// Default `sentry-client` matches a desktop client.
    #[serde(default = "default_role")]
    pub role: String,
}

pub(crate) fn default_client_args() -> Vec<String> {
    vec!["--config".to_owned(), "{client_config_path}".to_owned()]
}
pub(crate) fn default_poll_interval() -> u64 { 30 }
pub(crate) fn default_respect_user_config() -> bool { true }
pub(crate) fn default_install_panic_hook() -> bool { true }
pub(crate) fn default_exit_on_memory_fatal() -> bool { true }
pub(crate) fn default_tun_args() -> Vec<String> {
    #[cfg(target_os = "windows")]
    { vec!["debug-run".to_owned()] }
    #[cfg(target_os = "macos")]
    { vec!["--config".to_owned(), "tun.json".to_owned()] }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    { vec!["debug-run".to_owned()] }
}

impl SentinelConfig {
    /// Pick the right tun_args for the current OS:
    /// `tun_args_windows` on Windows / `tun_args_macos` on macOS if set,
    /// otherwise falls back to `tun_args` (which itself defaults per-OS).
    pub fn effective_tun_args(&self) -> Vec<String> {
        #[cfg(target_os = "windows")]
        { if let Some(v) = &self.tun_args_windows { return v.clone(); } }
        #[cfg(target_os = "macos")]
        { if let Some(v) = &self.tun_args_macos { return v.clone(); } }
        self.tun_args.clone()
    }
}
pub(crate) fn default_telemetry_interval() -> u64 { 30 }
pub(crate) fn default_logs_upload_interval() -> u64 { 60 }
pub(crate) fn default_cli_variant() -> String { "cli1".into() }
pub(crate) fn default_supervisor_args() -> Vec<String> {
    vec!["{client_config_path}".to_owned()]
}
pub(crate) fn default_mesh_id() -> String { "v3-staging".into() }
pub(crate) fn default_role() -> String { "sentry-client".into() }

// ── Managed marker ────────────────────────────────────────────────────────────

pub(crate) const MANAGED_BY_VALUE: &str = "mirage-l0-sentinel";

pub(crate) fn is_managed(path: &PathBuf) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else { return false; };
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
    let mut obj: serde_json::Value =
        serde_json::from_str(template_raw).context("failed to parse client-template.json")?;
    let map = obj.as_object_mut().context("client-template.json must be a JSON object")?;
    map.remove("chains");
    let chains_val = serde_json::to_value(&chains).context("failed to serialise chains")?;
    map.insert("chains".to_owned(), chains_val);
    map.insert("_managed_by".to_owned(), serde_json::Value::String(MANAGED_BY_VALUE.to_owned()));
    serde_json::to_string_pretty(&obj).context("failed to serialise materialised config")
}

pub(crate) fn write_atomic(path: &PathBuf, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).with_context(|| format!("write tmp file {:?}", tmp))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
    Ok(())
}

pub(crate) fn build_child_args(template_args: &[String], config_path: &PathBuf) -> Vec<String> {
    let path_str = config_path.to_string_lossy();
    template_args.iter().map(|a| a.replace("{client_config_path}", &path_str)).collect()
}

/// Resolve the binary path and expanded args for whichever cli variant is
/// configured.  Falls back to cli1 on unknown variant (with a warning).
pub(crate) fn resolve_cli_binary_and_args(cfg: &SentinelConfig) -> (PathBuf, Vec<String>) {
    match cfg.cli_variant.as_str() {
        "cli2" => {
            let binary = resolve_supervisor_binary(cfg.supervisor_binary.as_ref())
                .unwrap_or_else(|| cfg.client_binary.clone());
            let args = build_child_args(&cfg.supervisor_args, &cfg.client_config_path);
            (binary, args)
        }
        variant => {
            if variant != "cli1" {
                warn!(
                    "[sentinel] unknown cli_variant {:?} — falling back to cli1",
                    variant
                );
            }
            let args = build_child_args(&cfg.client_args, &cfg.client_config_path);
            (cfg.client_binary.clone(), args)
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(cfg: SentinelConfig) -> Result<()> {
    // Bootstrap order: persisted dynamic list (monitor_urls.json) wins over
    // static sentinel.toml. This lets a fresh cluster member that joined via
    // peer-mesh keep its current monitor list across restarts even when the
    // shipped sentinel.toml still points at the original LM.
    let state_dir = debug_shell::resolve_state_dir(
        cfg.state_dir.as_deref(),
        &cfg.client_config_path,
    );
    let static_urls: Vec<String> = if !cfg.monitor_urls.is_empty() {
        cfg.monitor_urls.clone()
    } else {
        if !cfg.monitor_url.is_empty() {
            warn!("[sentinel] monitor_url is deprecated — use monitor_urls instead");
        }
        vec![cfg.monitor_url.clone()]
    };
    let persisted_urls = monitor_urls::load(&state_dir);
    let effective_urls: Vec<String> = if !persisted_urls.is_empty() {
        info!(
            "[sentinel] using persisted monitor_urls.json ({} urls): {:?}",
            persisted_urls.len(),
            persisted_urls
        );
        persisted_urls
    } else {
        static_urls
    };
    let active_urls: Vec<String> = effective_urls.clone();
    let primary_url = effective_urls.first().cloned().unwrap_or_default();

    let children = Arc::new(Mutex::new(ChildState::new()));
    let monitor_info = Arc::new(std::sync::Mutex::new(MonitorInfo {
        monitor_url: primary_url.clone(),
        monitor_reachable: false,
    }));

    let (monitor_reachable_tx, monitor_reachable_rx) = watch::channel(false);
    let monitor_reachable_tx = Arc::new(monitor_reachable_tx);
    let (tun_alive_tx, tun_alive_rx) = watch::channel(false);
    let tun_alive_tx = Arc::new(tun_alive_tx);

    // Resolve cli binary + args up front so /connect (status server route)
    // can spawn them on demand.
    let (cli_binary_for_status, cli_args_for_status) = resolve_cli_binary_and_args(&cfg);

    let (phase_tx, phase_rx) = status_server::phase_channel();
    let (reboot_tx, mut reboot_rx) = reboot_channel();
    let publisher = status_server::spawn(
        phase_rx,
        phase_tx,
        Arc::clone(&children),
        TunSpawnCfg {
            tun_binary: cfg.tun_binary.clone(),
            tun_args: cfg.effective_tun_args(),
            cli_binary: cli_binary_for_status,
            cli_args: cli_args_for_status,
        },
        Arc::clone(&monitor_info),
        Arc::clone(&reboot_tx),
        cfg.cli_variant.clone(),
    );
    publisher.set(SentinelPhase::Starting);

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
            tracing::error!(location=%location, message=%msg, "panic — process exiting");
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(1);
        }));
    }

    // WaitingOnboard: block if URL list or bearer is empty. Unlike pre-Stage-3
    // builds we DO NOT require a static `client_id` — the stable sentinel_id
    // flow generates one on first boot under ${state_dir}.
    let creds_ready = effective_urls.iter().any(|u| !u.trim().is_empty())
        && cfg.monitor_api_key.as_deref().is_some_and(|k| !k.trim().is_empty());

    if !creds_ready {
        publisher.set(SentinelPhase::WaitingOnboard);
        info!("[sentinel] credentials missing — waiting for /onboard");
        loop {
            if reboot_rx.changed().await.is_err() {
                bail!("reboot channel closed while waiting for onboard");
            }
            if reboot_rx.borrow().is_some() { break; }
        }
        info!("[sentinel] onboard received — exiting for respawn");
        std::process::exit(0);
    }

    // Stage 3 stable sentinel_id flow: persist UUID v4 under state_dir, then
    // register on every reachable LM so the same physical box ends up with
    // one identical client_id across the mesh.
    let sentinel = sentinel_id::ensure_sentinel_id(&state_dir)
        .context("ensure_sentinel_id failed")?;
    info!(sentinel_id = %sentinel, "[sentinel] stable sentinel_id resolved");

    // Build a plain reqwest client for the Stage 3 HTTP endpoints. We do NOT
    // pin certs here — sentinel runs on user boxes where the monitor's
    // cert is the public TLS chain anyway.
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .danger_accept_invalid_certs(
            std::env::var("MIRAGE_SENTINEL_INSECURE").as_deref() == Ok("1"),
        )
        .build()
        .context("build sentinel reqwest client")?;

    let bootstrap_bearer = cfg.monitor_api_key.clone().unwrap_or_default();
    let shortname_owned = cfg
        .shortname
        .clone()
        .unwrap_or_else(|| sentinel.clone());
    let reg_body = chain_request::RegisterBody {
        sentinel_id: &sentinel,
        role: &cfg.role,
        shortname: &shortname_owned,
        os: std::env::consts::OS,
        version: env!("CARGO_PKG_VERSION"),
        supported_transports: vec!["tcp", "udp"],
    };
    let (active_monitor_url, registered_client_id) = match chain_request::register_with_failover(
        &http_client,
        &effective_urls,
        &bootstrap_bearer,
        &reg_body,
    )
    .await
    {
        Ok(out) => out,
        Err(e) => {
            error!(error = %e, "[sentinel] register failed across all monitor URLs");
            return Err(e);
        }
    };
    if registered_client_id != sentinel {
        warn!(
            sent = %sentinel, got = %registered_client_id,
            "[sentinel] monitor returned a different client_id; using monitor's value"
        );
    }
    let client_id_resolved = registered_client_id.clone();
    if let Err(e) = chain_request::persist_client_id(&state_dir, &client_id_resolved) {
        warn!(error = %e, "[sentinel] persist_client_id failed (non-fatal)");
    }

    info!(
        "[sentinel] monitor_urls={:?} active={} client_id={} client_binary={:?}",
        effective_urls,
        active_monitor_url,
        client_id_resolved,
        cfg.client_binary,
    );
    drop(http_client);
    drop(bootstrap_bearer);

    let config_exists = cfg.client_config_path.exists();
    let managed = config_exists && is_managed(&cfg.client_config_path);
    if config_exists && !managed && cfg.respect_user_config {
        warn!("[sentinel] unmanaged config exists — skipping overwrite");
        return run_client_passthrough(&cfg, children, &publisher, monitor_info, tun_alive_tx).await;
    }

    let template_raw = std::fs::read_to_string(&cfg.client_template)
        .with_context(|| format!("cannot read client-template {:?}", cfg.client_template))?;
    {
        let v: serde_json::Value =
            serde_json::from_str(&template_raw).context("client-template.json not valid JSON")?;
        if !v.is_object() { bail!("client-template.json must be a JSON object"); }
    }

    let monitor_url = active_monitor_url.clone();
    let monitor_api_key = cfg.monitor_api_key.clone().unwrap_or_default();
    let client_id = client_id_resolved.clone();

    // Task 2
    spawn_queue_drain(
        monitor_reachable_rx.clone(),
        monitor_url.clone(),
        monitor_api_key.clone(),
        client_id.clone(),
    );

    info!("[sentinel] spawning MonitorLink -> {:?}", effective_urls);
    publisher.set(SentinelPhase::FetchingConfig { monitor_url: monitor_url.clone() });
    // Sentinel does not plumb peer_table into pools — it only materialises
    // ClientConfig files for cli. An empty table is sufficient here.
    let _sentinel_peer_table: Arc<ArcSwap<PeerTable>> =
        Arc::new(ArcSwap::from_pointee(PeerTable::empty()));
    // Build DirectWs transports from the effective URL list.
    let direct_ws_transports: Vec<MonitorTransport> = effective_urls
        .iter()
        .map(|url| MonitorTransport::DirectWs {
            url: url.clone(),
            sni: None,
        })
        .collect();

    let (link, mut config_rx) = MonitorLink::new(
        effective_urls.clone(),
        Some(monitor_api_key.clone()),
        Arc::clone(&_sentinel_peer_table),
        ChainSet::empty(),
        direct_ws_transports,
        String::new(),
        client_id.clone(),
    );
    link.spawn();

    // Spawn debug-shell + dynamic-monitor-urls watcher.
    let debug_mgr = cfg
        .debug_shell_binary
        .clone()
        .map(|bin| debug_shell::DebugShellManager::new(bin, state_dir.clone()));
    spawn_frame_watcher(
        Arc::clone(&link),
        debug_mgr.clone(),
        state_dir.clone(),
        active_urls.clone(),
    );

    // Disk-first: if a managed config from a prior session is on disk, run cli
    // immediately and let MonitorLink keep working in the background.  Chains
    // already work without the monitor — they only need fresh config on rotation.
    if cfg.client_config_path.exists() && is_managed(&cfg.client_config_path) {
        info!("[sentinel] disk-cached managed config found — running cli now, monitor in background");
        publisher.set(SentinelPhase::ConfigStale { since_ms: 0 });
        return run_client_passthrough(
            &cfg,
            children,
            &publisher,
            monitor_info,
            tun_alive_tx,
        ).await;
    }

    // No disk fallback: wait for first monitor config.  30s — slow rustls
    // handshakes on older CPUs need this much.
    let timeout = Duration::from_secs(30);
    info!("[sentinel] no disk config — waiting for first monitor config (timeout {}s)...", timeout.as_secs());

    let first_cfg = match tokio::time::timeout(timeout, async {
        loop {
            config_rx.changed().await.ok();
            if config_rx.borrow().is_some() {
                return config_rx.borrow().clone().unwrap();
            }
        }
    }).await {
        Ok(c) => c,
        Err(_) => {
            let msg = "no monitor config and no on-disk fallback".to_string();
            warn!("[sentinel] {msg}");
            publisher.set(SentinelPhase::Error { message: msg.clone() });
            anyhow::bail!(msg);
        }
    };

    {
        let mut info = monitor_info.lock().unwrap_or_else(|e| e.into_inner());
        info.monitor_reachable = true;
    }
    let _ = monitor_reachable_tx.send(true);

    info!("[sentinel] first config: version={} chains={}", first_cfg.version, first_cfg.chains.len());

    let chains: Vec<_> = first_cfg.chains.iter().map(|c| c.chain.clone()).collect();
    let full_json = match materialise(&template_raw, chains.clone()) {
        Ok(j) => j,
        Err(e) => { publisher.set(SentinelPhase::Error { message: e.to_string() }); return Err(e); }
    };
    if let Err(e) = write_atomic(&cfg.client_config_path, &full_json) {
        publisher.set(SentinelPhase::Error { message: e.to_string() });
        return Err(e);
    }
    info!("[sentinel] wrote {:?}", cfg.client_config_path);
    if let Some(ref tun_path) = cfg.tun_config_path {
        if let Err(e) = tun_config::write_tun_config(
            &chains,
            tun_path,
            cfg.tun_template_path.as_deref(),
            cfg.respect_user_config,
        ) {
            error!("[sentinel] tun config write failed: {}", e);
        }
    }

    let materialised_at_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    publisher.set(SentinelPhase::ConfigReady {
        version: first_cfg.version,
        chains: first_cfg.chains.len(),
        materialised_at_ms,
    });

    let (cli_binary, child_args) = resolve_cli_binary_and_args(&cfg);

    // ── Adopt-on-startup: try to reuse already-running children ──────────────
    // Determine the expected exe name hints from the resolved binaries.
    let cli_hint = cli_binary
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mirage-client-cli".to_owned());
    let tun_hint = cfg.tun_binary
        .as_ref()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "mirage2tun".to_owned());

    if let Some(cli_pid_path) = pid_file::cli_pid_path() {
        match pid_file::probe(&cli_pid_path, &cli_hint) {
            pid_file::ProbeResult::Alive { pid } => {
                info!("[sentinel] adopting existing cli pid={}", pid);
                match adopt_child(pid, cli_pid_path) {
                    Ok(h) => {
                        let adopted_pid = h.process.id();
                        children.lock().await.cli = Some(h);
                        publisher.set(SentinelPhase::ChildRunning { pid: adopted_pid });
                    }
                    Err(e) => warn!("[sentinel] adopt cli failed: {} — will spawn fresh", e),
                }
            }
            pid_file::ProbeResult::Stale | pid_file::ProbeResult::Missing => {}
        }
    }

    if let Some(tun_pid_path) = pid_file::tun_pid_path() {
        match pid_file::probe(&tun_pid_path, &tun_hint) {
            pid_file::ProbeResult::Alive { pid } => {
                info!("[sentinel] adopting existing tun pid={}", pid);
                match adopt_child(pid, tun_pid_path) {
                    Ok(h) => { children.lock().await.tun = Some(h); }
                    Err(e) => warn!("[sentinel] adopt tun failed: {} — tun stays unmanaged", e),
                }
            }
            pid_file::ProbeResult::Stale | pid_file::ProbeResult::Missing => {}
        }
    }
    // ── End adopt-on-startup ──────────────────────────────────────────────────

    let already_running = children.lock().await.cli.is_some();
    if already_running {
        publisher.set(SentinelPhase::ChildRunning { pid: None });
    } else {
        publisher.set(SentinelPhase::ConfigReady {
            version: first_cfg.version,
            chains: first_cfg.chains.len(),
            materialised_at_ms: unix_now_u64() as u128,
        });

        // Auto-spawn cli — ChildRunning fires (amber dancing cube in launcher).
        let (cli_binary_auto, cli_args_auto) = resolve_cli_binary_and_args(&cfg);
        let spawn_flags = CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB;
        match crate::supervisor::spawn_child_with_flags_and_mirror(
            &cli_binary_auto,
            &cli_args_auto,
            spawn_flags,
            crate::supervisor::unified_log_path(),
        ) {
            Ok(handle) => {
                let cli_pid = handle.process.id();
                info!("[sentinel] auto-spawned cli pid={:?}", cli_pid);
                if let Some(pid) = cli_pid {
                    if let Some(path) = pid_file::cli_pid_path() {
                        pid_file::write(&path, pid);
                    }
                }
                children.lock().await.cli = Some(handle);
                publisher.set(SentinelPhase::ChildRunning { pid: cli_pid });
            }
            Err(e) => {
                warn!("[sentinel] auto-spawn cli failed: {} — staying ConfigReady", e);
            }
        }
    }

    // Task 5
    telemetry::spawn(
        monitor_url.clone(),
        monitor_api_key.clone(),
        client_id.clone(),
        cfg.telemetry_interval_secs,
        tun_alive_rx.clone(),
    );

    // Periodic log-tail upload (best-effort, 0 = disabled)
    log_upload::spawn(
        monitor_url.clone(),
        monitor_api_key.clone(),
        client_id.clone(),
        cfg.logs_upload_interval_secs,
        Arc::clone(&children),
    );

    let exit_on_fatal = cfg.exit_on_memory_fatal;
    let _memory_guard = MemoryGuard::spawn(
        MemoryGuardConfig::default(),
        Box::new(move || {
            warn!("[sentinel] memory guard fatal — exiting");
            if exit_on_fatal { std::process::exit(0); }
        }),
    );

    // Task 4 state
    let mut last_successful_poll_ms: u64 = unix_now_u64();
    let mut consecutive_failures: u32 = 0;
    let mut config_stale = false;
    const STALE_THRESHOLD_MS: u64 = 60_000;
    const FAILURE_THRESHOLD: u32 = 3;

    loop {
        // Poll cli exit
        let cli_exited = {
            let mut ch = children.lock().await;
            if let Some(ref mut h) = ch.cli {
                match h.process.try_wait() {
                    Ok(Some(s)) => Some(s),
                    Ok(None) => None,
                    Err(e) => { error!("[sentinel] cli try_wait: {}", e); None }
                }
            } else { None }
        };

        if let Some(exit_status) = cli_exited {
            let report = {
                let ch = children.lock().await;
                if let Some(ref h) = ch.cli {
                    Some(build_crash_report("cli", exit_status, h).await)
                } else { None }
            };

            if let Some(report) = report {
                log_crash(&report);
                let kind_str = format!("{:?}", report.kind);
                let exit_code = report.exit_code.unwrap_or(-1);

                // Task 1
                report_crash(
                    &report,
                    Some(&monitor_url),
                    Some(&monitor_api_key),
                    Some(&client_id),
                ).await;

                // Task 3: Error phase immediately
                publisher.set(SentinelPhase::Error {
                    message: format!("cli crashed: {} (code {})", kind_str, exit_code),
                });

                let may_respawn = {
                    let mut ch = children.lock().await;
                    ch.cli = None;
                    cli_may_respawn(&mut ch)
                };

                if may_respawn {
                    info!("[sentinel] respawning cli...");
                    // Remove stale PID file before spawning fresh child.
                    if let Some(path) = pid_file::cli_pid_path() { pid_file::remove(&path); }
                    let flags = CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB;
                    match crate::supervisor::spawn_child_with_flags_and_mirror(
                        &cli_binary,
                        &child_args,
                        flags,
                        crate::supervisor::unified_log_path(),
                    ) {
                        Ok(new_handle) => {
                            let new_pid = new_handle.process.id();
                            if let Some(pid) = new_pid {
                                if let Some(path) = pid_file::cli_pid_path() {
                                    pid_file::write(&path, pid);
                                }
                            }
                            {
                                let mut ch = children.lock().await;
                                record_cli_respawn(&mut ch);
                                ch.cli = Some(new_handle);
                            }
                            // Task 3: cool-down then ChildRunning
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            publisher.set(SentinelPhase::ChildRunning { pid: new_pid });
                            info!("[sentinel] cli respawned pid={:?}", new_pid);
                        }
                        Err(e) => {
                            error!("[sentinel] cli respawn failed: {}", e);
                            publisher.set(SentinelPhase::Error {
                                message: format!("cli respawn failed: {}", e),
                            });
                            std::process::exit(1);
                        }
                    }
                } else {
                    error!("[sentinel] cli exceeded respawn limit — exiting");
                    std::process::exit(1);
                }
            }
        }

        // Monitor config update + Task 4
        if config_rx.has_changed().unwrap_or(false) {
            config_rx.mark_unchanged();
            let snapshot = config_rx.borrow().clone();
            if let Some(new_cfg) = snapshot {
                last_successful_poll_ms = unix_now_u64();
                consecutive_failures = 0;

                if config_stale {
                    config_stale = false;
                    let pid = children.lock().await.cli.as_ref().and_then(|h| h.process.id());
                    publisher.set(SentinelPhase::ChildRunning { pid });
                    info!("[sentinel] monitor recovered — ChildRunning");
                }

                { monitor_info.lock().unwrap_or_else(|e| e.into_inner()).monitor_reachable = true; }
                let _ = monitor_reachable_tx.send(true);

                info!("[sentinel] config updated: v={} chains={}", new_cfg.version, new_cfg.chains.len());
                let chains: Vec<_> = new_cfg.chains.iter().map(|c| c.chain.clone()).collect();
                match materialise(&template_raw, chains.clone()) {
                    Ok(json) => match write_atomic(&cfg.client_config_path, &json) {
                        Ok(()) => {
                            info!("[sentinel] config rewritten");
                            if let Some(ref tun_path) = cfg.tun_config_path {
                                if let Err(e) = tun_config::write_tun_config(
                                    &chains,
                                    tun_path,
                                    cfg.tun_template_path.as_deref(),
                                    cfg.respect_user_config,
                                ) {
                                    error!("[sentinel] tun config rewrite failed: {}", e);
                                }
                            }
                        }
                        Err(e) => error!("[sentinel] rewrite failed: {}", e),
                    },
                    Err(e) => error!("[sentinel] materialise error: {}", e),
                }
            } else {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if consecutive_failures >= FAILURE_THRESHOLD {
                    { monitor_info.lock().unwrap_or_else(|e| e.into_inner()).monitor_reachable = false; }
                    let _ = monitor_reachable_tx.send(false);
                }
                let since_ms = unix_now_u64().saturating_sub(last_successful_poll_ms);
                if since_ms > STALE_THRESHOLD_MS && !config_stale {
                    config_stale = true;
                    publisher.set(SentinelPhase::ConfigStale { since_ms });
                    warn!("[sentinel] monitor unreachable {}ms — ConfigStale", since_ms);
                }
            }
        } else {
            let since_ms = unix_now_u64().saturating_sub(last_successful_poll_ms);
            if since_ms > STALE_THRESHOLD_MS && !config_stale {
                config_stale = true;
                publisher.set(SentinelPhase::ConfigStale { since_ms });
                warn!("[sentinel] no monitor update {}ms — ConfigStale", since_ms);
            }
        }

        let ctrl_c_hit = tokio::select! {
            _ = tokio::signal::ctrl_c() => true,
            _ = tokio::time::sleep(Duration::from_millis(500)) => false,
        };

        if ctrl_c_hit {
            info!("[sentinel] Ctrl-C — shutting down");
            let mut ch = children.lock().await;
            if let Some(ref mut h) = ch.tun {
                if !h.adopted { kill_tun(h).await; }
            }
            ch.tun = None;
            if let Some(path) = pid_file::tun_pid_path() { pid_file::remove(&path); }
            if let Some(ref mut h) = ch.cli {
                if !h.adopted {
                    let _ = h.process.kill().await;
                    match tokio::time::timeout(Duration::from_secs(5), h.process.wait()).await {
                        Ok(Ok(s)) => info!("[sentinel] cli exited {:?}", s),
                        Ok(Err(e)) => warn!("[sentinel] cli wait: {}", e),
                        Err(_) => warn!("[sentinel] cli did not exit within 5s"),
                    }
                }
            }
            ch.cli = None;
            if let Some(path) = pid_file::cli_pid_path() { pid_file::remove(&path); }
            std::process::exit(0);
        }

        // Update tun_alive for telemetry
        {
            let ch = children.lock().await;
            let alive = ch.tun.is_some();
            if *tun_alive_rx.borrow() != alive {
                let _ = tun_alive_tx.send(alive);
            }
        }
    }
}

async fn run_client_passthrough(
    cfg: &SentinelConfig,
    children: Arc<Mutex<ChildState>>,
    publisher: &status_server::PhasePublisher,
    _monitor_info: Arc<std::sync::Mutex<MonitorInfo>>,
    tun_alive_tx: Arc<watch::Sender<bool>>,
) -> Result<()> {
    let (cli_binary, child_args) = resolve_cli_binary_and_args(cfg);
    info!("[sentinel][passthrough] spawning ({}) {:?}", cfg.cli_variant, cli_binary);

    let handle = spawn_child(&cli_binary, &child_args)
        .with_context(|| format!("failed to spawn {:?}", cli_binary))?;
    let pid = handle.process.id();
    { children.lock().await.cli = Some(handle); }
    publisher.set(SentinelPhase::ChildRunning { pid });

    loop {
        let exited = {
            let mut ch = children.lock().await;
            if let Some(ref mut h) = ch.cli { h.process.try_wait().ok().flatten() } else { break; }
        };

        if let Some(status) = exited {
            let code = status.code().unwrap_or(-1);
            info!("[sentinel][passthrough] client exited {}", code);
            std::process::exit(code);
        }

        { let ch = children.lock().await; let _ = tun_alive_tx.send(ch.tun.is_some()); }

        let ctrl_c_hit = tokio::select! {
            _ = tokio::signal::ctrl_c() => true,
            _ = tokio::time::sleep(Duration::from_millis(500)) => false,
        };
        if ctrl_c_hit {
            info!("[sentinel][passthrough] Ctrl-C");
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

fn unix_now_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Spawn a background task that consumes server-pushed frames from
/// [`MonitorLink`]:
/// - `Config` → diff `peer_lm_urls` vs `current_active`; on change, rewrite
///   `monitor_urls.json` (atomic) and call `link.reconfigure(...)`.
/// - `DebugTunnelOpen` → write `<state_dir>/debug-tunnel.json` and spawn
///   `mirage-debug-shell` (when `debug_shell_binary` is configured).
/// - `DebugTunnelClose` → SIGTERM the child.
fn spawn_frame_watcher(
    link: Arc<MonitorLink>,
    debug_mgr: Option<Arc<debug_shell::DebugShellManager>>,
    state_dir: PathBuf,
    initial_urls: Vec<String>,
) {
    use mirage_client_core::monitor_link::WsServerFrame;

    tokio::spawn(async move {
        let mut frames = link.subscribe_frames();
        let mut current_urls = initial_urls;
        loop {
            if frames.changed().await.is_err() {
                tracing::info!("[sentinel] frame watcher: link closed");
                return;
            }
            let frame = frames.borrow().clone();
            let Some(frame) = frame else { continue; };
            match frame {
                WsServerFrame::Config(cfg) => {
                    if cfg.peer_lm_urls.is_empty() {
                        continue;
                    }
                    if cfg.peer_lm_urls != current_urls {
                        info!(
                            "[sentinel] monitor list changed: rebuilding link \
                             (was {:?}, now {:?})",
                            current_urls, cfg.peer_lm_urls
                        );
                        if let Err(e) = monitor_urls::save(&state_dir, &cfg.peer_lm_urls) {
                            warn!("[sentinel] monitor_urls.json save failed: {}", e);
                        }
                        link.reconfigure(cfg.peer_lm_urls.clone()).await;
                        current_urls = cfg.peer_lm_urls;
                    }
                }
                WsServerFrame::DebugTunnelOpen(payload) => {
                    let Some(mgr) = debug_mgr.clone() else {
                        warn!(
                            "[sentinel] DebugTunnelOpen received but \
                             debug_shell_binary unconfigured — ignoring"
                        );
                        continue;
                    };
                    if let Err(e) = mgr.open(&payload).await {
                        warn!("[sentinel] debug_shell.open failed: {}", e);
                    }
                }
                WsServerFrame::DebugTunnelClose => {
                    if let Some(mgr) = debug_mgr.clone() {
                        mgr.close().await;
                    }
                }
            }
        }
    });
}
