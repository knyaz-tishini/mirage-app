//! GUI↔engine link.
//!
//! `EngineLink` is the seam between the uzor GUI and the Mirage data-plane. The
//! desktop impl (`LocalHttp`) owns the multi-process L0 bundle exactly like the
//! proven `mirage-launcher`: it generates the configs, spawns
//! `mirage-l0-orchestrator` (which brings up sentinel → cli2 → tun), and drives
//! it over loopback HTTP (sentinel `:19420`). A future `TaskMode` impl (in-process
//! tokio tasks inside the NE extension) will satisfy the same trait for
//! iOS / Mac App Store without touching the GUI.
//!
//! Off by default — only constructed when `--live` is passed (so a plain
//! UI-development run never spawns the backend or opens sockets).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SENTINEL: &str = "http://127.0.0.1:19420";

/// Snapshot of the engine state the GUI reflects. `online=false` ⇒ sentinel
/// control port unreachable (backend not up).
// Only `tun_alive` drives the phase today; the rest feed the Connect-tab live
// status surfacing (next slice).
#[allow(dead_code)]
#[derive(Clone, Default)]
pub struct EngineStatus {
    pub online: bool,
    /// Data-plane (tun) is alive → the tunnel is up. This is the connect truth.
    pub tun_alive: bool,
    pub cli_alive: bool,
    /// Sentinel bootstrap phase (starting / config_ready / error / …).
    pub phase: String,
    pub monitor_reachable: bool,
    pub error: Option<String>,
}

/// GUI↔engine seam. `LocalHttp` now (desktop multi-process); `TaskMode` later
/// (sandboxed in-process) will implement the same trait.
pub trait EngineLink: Send + Sync {
    fn status(&self) -> EngineStatus;
    fn connect(&self);
    fn disconnect(&self);
}

/// Desktop link: owns the orchestrator process tree + polls sentinel over HTTP.
pub struct LocalHttp {
    status: Arc<Mutex<EngineStatus>>,
    orchestrator: Option<Child>,
}

impl LocalHttp {
    /// Generate configs (once), spawn the orchestrator, start the status poller.
    pub fn start() -> Self {
        ensure_configs();
        let orchestrator = match spawn_orchestrator() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("spawn orchestrator failed: {e}");
                None
            }
        };
        let status = Arc::new(Mutex::new(EngineStatus::default()));
        spawn_poller(status.clone());
        Self { status, orchestrator }
    }
}

impl EngineLink for LocalHttp {
    fn status(&self) -> EngineStatus {
        self.status.lock().unwrap().clone()
    }
    fn connect(&self) {
        fire("/connect");
    }
    fn disconnect(&self) {
        fire("/disconnect");
    }
}

impl Drop for LocalHttp {
    fn drop(&mut self) {
        if let Some(mut c) = self.orchestrator.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        reap_orphans();
    }
}

// ── HTTP poll + control ────────────────────────────────────────────────────────

fn spawn_poller(status: Arc<Mutex<EngineStatus>>) {
    std::thread::Builder::new()
        .name("mirage-engine-poll".into())
        .spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .expect("reqwest client");
            loop {
                let st = match client.get(format!("{SENTINEL}/status")).send() {
                    Ok(r) => r.text().map(|b| parse_status(&b)).unwrap_or_default(),
                    Err(_) => EngineStatus::default(), // backend down
                };
                *status.lock().unwrap() = st;
                std::thread::sleep(Duration::from_millis(700));
            }
        })
        .ok();
}

/// Fire-and-forget POST to a sentinel control endpoint.
fn fire(path: &'static str) {
    std::thread::spawn(move || {
        if let Ok(c) = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
        {
            let _ = c.post(format!("{SENTINEL}{path}")).send();
        }
    });
}

fn parse_status(body: &str) -> EngineStatus {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return EngineStatus::default(),
    };
    let phase = v.get("phase").and_then(|x| x.as_str()).unwrap_or("").to_string();
    EngineStatus {
        online: true,
        tun_alive: v.pointer("/tun/alive").and_then(|x| x.as_bool()).unwrap_or(false),
        cli_alive: v.pointer("/cli/alive").and_then(|x| x.as_bool()).unwrap_or(false),
        monitor_reachable: v.get("monitor_reachable").and_then(|x| x.as_bool()).unwrap_or(false),
        error: if phase == "error" {
            v.get("message").and_then(|x| x.as_str()).map(str::to_string)
        } else {
            None
        },
        phase,
    }
}

// ── Process lifecycle (ported 1:1 from mirage-launcher) ──────────────────────────

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .expect("current_exe always valid")
        .parent()
        .expect("exe always has parent")
        .to_path_buf()
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Generate the three config files next to the exe if they don't exist yet.
/// Existing files are left untouched — so a hand-patched `sentinel.toml` (live
/// monitor URL + Client credential) survives.
fn ensure_configs() {
    ensure_sentinel_toml();
    ensure_orchestrator_toml();
    ensure_client_template_cli2();
}

fn ensure_sentinel_toml() {
    let dir = exe_dir();
    let toml_path = dir.join("sentinel.toml");
    if toml_path.exists() {
        return;
    }
    let dir_str = dir.to_string_lossy().replace('\\', "/");
    let client_config = format!("{dir_str}/client-config.json");
    let exe = std::env::consts::EXE_SUFFIX;
    let cli_bin = format!("{dir_str}/mirage-chain-supervisor{exe}");
    let tun_bin = format!("{dir_str}/mirage2tun{exe}");
    let content = format!(
        r#"# Auto-generated by mirage-client-gui. Patch monitor_url + monitor_api_key
# (Client role) for a live connect.

monitor_url = "http://127.0.0.1:19410"
# monitor_api_key = "..."

client_config_path = "{client_config}"
client_template = "{client_config}"

client_binary = "{cli_bin}"
client_args   = ["--config", "{{client_config_path}}"]

tun_binary = "{tun_bin}"
tun_args   = ["debug-run"]

poll_interval_secs = 30
respect_user_config = true

# Orchestrator owns the cli — sentinel must not also spawn it.
spawn_cli = false
"#
    );
    if let Err(e) = std::fs::write(&toml_path, content) {
        tracing::warn!("could not write sentinel.toml: {e}");
    }
}

fn ensure_orchestrator_toml() {
    let dir = exe_dir();
    let toml_path = dir.join("orchestrator.toml");
    if toml_path.exists() {
        return;
    }
    let dir_str = dir.to_string_lossy().replace('\\', "/");
    let client_config = format!("{dir_str}/client-config.json");
    let exe = std::env::consts::EXE_SUFFIX;
    let sentinel_bin = format!("{dir_str}/mirage-l0-sentinel{exe}");
    let sentinel_cfg = format!("{dir_str}/sentinel.toml");
    let sup_bin = format!("{dir_str}/mirage-chain-supervisor{exe}");
    let content = format!(
        "# Auto-generated by mirage-client-gui.\n\
         \n\
         [orchestrator]\n\
         mode = \"process\"\n\
         \n\
         [[component]]\n\
         name    = \"sentinel\"\n\
         kind    = \"sentinel\"\n\
         binary  = \"{sentinel_bin}\"\n\
         args    = [\"--config\", \"{sentinel_cfg}\"]\n\
         restart = \"on-failure\"\n\
         \n\
         [[component]]\n\
         name          = \"data-plane\"\n\
         kind          = \"generic\"\n\
         binary        = \"{sup_bin}\"\n\
         args          = [\"{client_config}\"]\n\
         restart       = \"on-failure\"\n\
         wait_for_file = \"{client_config}\"\n"
    );
    if let Err(e) = std::fs::write(&toml_path, content) {
        tracing::warn!("could not write orchestrator.toml: {e}");
    }
}

fn ensure_client_template_cli2() {
    let dir = exe_dir();
    let cfg_path = dir.join("client-config.json");
    if cfg_path.exists() {
        return;
    }
    let dir_str = dir.to_string_lossy().replace('\\', "/");
    let exe = std::env::consts::EXE_SUFFIX;
    let worker_bin = format!("{dir_str}/mirage-chain-worker{exe}");
    let content = format!(
        "{{\n  \"client_id\": \"mirage-desktop\",\n  \"mesh_id\": \"v3-staging\",\n  \
         \"chains\": [],\n  \"routing\": {{}},\n  \"psk\": null,\n  \
         \"supervisor\": {{\n    \"worker_binary\": \"{worker_bin}\",\n    \
         \"reload_poll_secs\": 5\n  }}\n}}\n"
    );
    if let Err(e) = std::fs::write(&cfg_path, content) {
        tracing::warn!("could not write client-config.json template: {e}");
    }
}

fn spawn_orchestrator() -> std::io::Result<Child> {
    reap_orphans();
    let dir = exe_dir();
    let config = dir.join("orchestrator.toml");
    let log_path = dir.join("mirage-client-gui-stack.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log2 = log.try_clone()?;

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let orch = dir.join("mirage-l0-orchestrator.exe");
        Command::new(&orch)
            .arg("--config")
            .arg(&config)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
    }
    #[cfg(not(windows))]
    {
        let orch = dir.join("mirage-l0-orchestrator");
        Command::new(&orch)
            .arg("--config")
            .arg(&config)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2))
            .spawn()
    }
}

/// Best-effort kill of leftover stack processes from a crashed run.
fn reap_orphans() {
    #[cfg(unix)]
    {
        for name in &[
            "mirage-l0-orchestrator",
            "mirage-l0-sentinel",
            "mirage-chain-supervisor",
            "mirage-chain-worker",
        ] {
            let _ = Command::new("pkill").arg("-9").arg("-x").arg(name).status();
        }
        // tun runs as root — non-prompting sudo (cached); skip if no cache.
        let _ = Command::new("sudo")
            .args(["-n", "pkill", "-9", "-x", "mirage2tun"])
            .status();
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        for name in &[
            "mirage-l0-orchestrator.exe",
            "mirage-l0-sentinel.exe",
            "mirage2tun.exe",
            "mirage-chain-supervisor.exe",
            "mirage-chain-worker.exe",
        ] {
            let _ = Command::new("taskkill")
                .args(["/F", "/IM", name])
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        }
    }
    std::thread::sleep(Duration::from_millis(300));
}
