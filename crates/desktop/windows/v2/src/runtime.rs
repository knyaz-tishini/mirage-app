use std::ffi::OsStr;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use mirage_core::config::ClientConfig;

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessStatus {
    Stopped,
    Starting,
    Running,
    Error(String),
}

#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct ProcessStats {
    pub active_chain_id: String,
    pub mode_socks5: bool,
    pub mode_http: bool,
    pub mode_tun: bool,
    pub pool_size: usize,
    pub pool_idle: usize,
    pub pool_leased: usize,
    pub pool_reconnecting: usize,
    pub cover_traffic_enabled: bool,
    pub config_version: u64,
}

/// Info needed to poll a running client process from an async task.
#[derive(Clone)]
pub struct PollInfo {
    pub http_client: reqwest::Client,
    pub management_url: String,
    pub management_key: String,
}

// ── raw FFI for ShellExecuteExW / GetProcessId ────────────────────────────────

#[repr(C)]
struct ShellExecuteInfoW {
    cb_size: u32,
    f_mask: u32,
    hwnd: *mut std::ffi::c_void,
    lp_verb: *const u16,
    lp_file: *const u16,
    lp_parameters: *const u16,
    lp_directory: *const u16,
    n_show: i32,
    h_inst_app: *mut std::ffi::c_void,
    lp_id_list: *mut std::ffi::c_void,
    lp_class: *const u16,
    h_key_class: *mut std::ffi::c_void,
    dw_hot_key: u32,
    h_icon_or_monitor: *mut std::ffi::c_void,
    h_process: *mut std::ffi::c_void,
}

const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0000_0040;
const SW_HIDE: i32 = 0;

#[link(name = "Shell32")]
extern "system" {
    fn ShellExecuteExW(p_exec_info: *mut ShellExecuteInfoW) -> i32;
}

#[link(name = "Kernel32")]
extern "system" {
    fn GetLastError() -> u32;
    fn GetProcessId(h_process: *mut std::ffi::c_void) -> u32;
    fn CloseHandle(h_object: *mut std::ffi::c_void) -> i32;
    fn OpenProcess(
        dw_desired_access: u32,
        b_inherit_handle: i32,
        dw_process_id: u32,
    ) -> *mut std::ffi::c_void;
    fn TerminateProcess(h_process: *mut std::ffi::c_void, u_exit_code: u32) -> i32;
    fn GetExitCodeProcess(
        h_process: *mut std::ffi::c_void,
        lp_exit_code: *mut u32,
    ) -> i32;
}

const PROCESS_TERMINATE: u32 = 0x0001;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const STILL_ACTIVE: u32 = 259;

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Spawn `exe` with `args` elevated via ShellExecuteExW "runas".
/// Returns the PID of the elevated process, or an error string.
fn spawn_elevated(exe: &Path, args: &str) -> Result<u32, String> {
    let verb = to_wide("runas");
    let file = to_wide(&exe.to_string_lossy());
    let params = to_wide(args);

    let mut info: ShellExecuteInfoW = unsafe { mem::zeroed() };
    info.cb_size = mem::size_of::<ShellExecuteInfoW>() as u32;
    info.f_mask = SEE_MASK_NOCLOSEPROCESS;
    info.lp_verb = verb.as_ptr();
    info.lp_file = file.as_ptr();
    info.lp_parameters = params.as_ptr();
    info.n_show = SW_HIDE;

    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        return Err(format!("ShellExecuteExW failed (GetLastError={err})"));
    }

    let pid = if !info.h_process.is_null() {
        let pid = unsafe { GetProcessId(info.h_process) };
        unsafe { CloseHandle(info.h_process) };
        pid
    } else {
        0
    };

    Ok(pid)
}

/// Kill process by PID. Best-effort.
fn kill_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

/// Returns true if a process with this PID is still running.
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}

// ── ClientProcess ─────────────────────────────────────────────────────────────

pub struct ClientProcess {
    pid: u32,
    config_path: PathBuf,
    management_url: String,
    management_key: String,
    http_client: reqwest::Client,
}

impl ClientProcess {
    /// Spawn `mirage-client.exe` elevated (UAC prompt) with the given config.
    /// Writes config to a temp file, ensures `management_key` is set.
    /// stdout/stderr are not captured (elevated process runs in a separate
    /// security context); use the management HTTP API for status/logs.
    pub async fn start(
        mut config: ClientConfig,
        client_exe: &Path,
        socks5_bind: &str,
    ) -> Result<Self, String> {
        // Ensure management key exists.
        let mgmt_key = config.management_key.clone().unwrap_or_else(|| {
            use rand::Rng;
            rand::thread_rng()
                .sample_iter(&rand::distributions::Alphanumeric)
                .take(32)
                .map(char::from)
                .collect()
        });
        config.management_key = Some(mgmt_key.clone());

        // Management address — always force 19483 regardless of config.
        let mgmt_addr = "127.0.0.1:19483".to_string();
        config.management_addr = Some(mgmt_addr.clone());

        // Write config to temp file.
        let config_path = std::env::temp_dir().join("mirage-light-config.json");
        let json = serde_json::to_string_pretty(&config)
            .map_err(|e| format!("serialize config: {e}"))?;
        std::fs::write(&config_path, &json).map_err(|e| format!("write config: {e}"))?;

        // Build args string for ShellExecuteExW (absolute paths required).
        let abs_config = config_path.canonicalize().unwrap_or(config_path.clone());
        let args = format!(
            "--config \"{}\" --bind {}",
            abs_config.to_string_lossy(), socks5_bind
        );

        // Spawn elevated — triggers UAC prompt.
        // Must run on a blocking thread so the UAC dialog can display.
        let exe_for_spawn = client_exe.to_path_buf();
        let args_for_spawn = args.clone();
        let pid = tokio::task::spawn_blocking(move || {
            spawn_elevated(&exe_for_spawn, &args_for_spawn)
        })
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?
        .map_err(|e| format!("elevated spawn: {e}"))?;

        let connect_addr = mgmt_addr.replace("0.0.0.0", "127.0.0.1");
        let management_url = format!("http://{connect_addr}");
        let http_client = reqwest::Client::new();

        let proc = Self {
            pid,
            config_path,
            management_url,
            management_key: mgmt_key,
            http_client,
        };

        // Wait for health check (up to 15 seconds).
        proc.wait_healthy(15).await?;

        Ok(proc)
    }

    /// Spawn `mirage-client.exe` non-elevated (no UAC) for TUN mode.
    /// TUN adapter is managed by mirage2tun service, so client only needs SOCKS5.
    pub async fn start_simple(
        mut config: ClientConfig,
        client_exe: &Path,
        socks5_bind: &str,
    ) -> Result<Self, String> {
        // Ensure management key exists.
        let mgmt_key = config.management_key.clone().unwrap_or_else(|| {
            use rand::Rng;
            rand::thread_rng()
                .sample_iter(&rand::distributions::Alphanumeric)
                .take(32)
                .map(char::from)
                .collect()
        });
        config.management_key = Some(mgmt_key.clone());

        // Management address — always force 19483.
        let mgmt_addr = "127.0.0.1:19483".to_string();
        config.management_addr = Some(mgmt_addr.clone());

        // Disable TUN in config — mirage2tun handles it externally.
        config.tun.enabled = false;

        // Write config to temp file.
        let config_path = std::env::temp_dir().join("mirage-light-config.json");
        let json = serde_json::to_string_pretty(&config)
            .map_err(|e| format!("serialize config: {e}"))?;
        std::fs::write(&config_path, &json).map_err(|e| format!("write config: {e}"))?;

        let abs_config = config_path.canonicalize().unwrap_or(config_path.clone());
        let exe_path = client_exe.to_path_buf();
        let bind = socks5_bind.to_owned();
        let abs_config_str = abs_config.to_string_lossy().to_string();

        // Spawn non-elevated — no UAC prompt needed for SOCKS5-only client.
        let pid = tokio::task::spawn_blocking(move || -> Result<u32, String> {
            let child = std::process::Command::new(&exe_path)
                .args(["--config", &abs_config_str, "--bind", &bind])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("spawn mirage-client: {e}"))?;
            Ok(child.id())
        })
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?
        .map_err(|e| format!("spawn: {e}"))?;

        let connect_addr = mgmt_addr.replace("0.0.0.0", "127.0.0.1");
        let management_url = format!("http://{connect_addr}");
        let http_client = reqwest::Client::new();

        let proc = Self {
            pid,
            config_path,
            management_url,
            management_key: mgmt_key,
            http_client,
        };

        // Wait for health check (up to 15 seconds).
        proc.wait_healthy(15).await?;

        Ok(proc)
    }

    async fn wait_healthy(&self, timeout_secs: u64) -> Result<(), String> {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            if tokio::time::Instant::now() > deadline {
                return Err("mirage-client health check timeout".into());
            }
            if self.health_check().await.unwrap_or(false) {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    pub async fn health_check(&self) -> Result<bool, String> {
        let url = format!("{}/admin/health", self.management_url);
        match self.http_client.get(&url).send().await {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(_) => Ok(false),
        }
    }

    /// Returns true if the elevated process is still running.
    pub fn is_alive(&mut self) -> bool {
        pid_alive(self.pid)
    }

    pub async fn stop(&mut self) {
        kill_pid(self.pid);
        let _ = std::fs::remove_file(&self.config_path);
    }

    /// Clone the HTTP details needed for background polling.
    pub fn poll_info(&self) -> PollInfo {
        PollInfo {
            http_client: self.http_client.clone(),
            management_url: self.management_url.clone(),
            management_key: self.management_key.clone(),
        }
    }
}

impl Drop for ClientProcess {
    fn drop(&mut self) {
        kill_pid(self.pid);
    }
}

/// Find `mirage-client.exe` by searching common locations.
pub fn find_client_exe() -> Option<PathBuf> {
    // 1. Same directory as the current executable.
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(Path::new("."));
        let candidate = dir.join("mirage-client.exe");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // 2. Relative to the repo structure (canonicalize for ShellExecuteExW).
    let repo_paths = [
        "../mirage2web/target/release/mirage-client.exe",
        "../mirage2web/target/debug/mirage-client.exe",
    ];
    for p in &repo_paths {
        let path = PathBuf::from(p);
        if path.exists() {
            if let Ok(abs) = path.canonicalize() {
                return Some(abs);
            }
            return Some(path);
        }
    }

    // 3. Check PATH via `where`.
    if let Ok(output) = std::process::Command::new("where")
        .arg("mirage-client.exe")
        .output()
    {
        if output.status.success() {
            let s = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = s.lines().next() {
                let p = PathBuf::from(line.trim());
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }

    None
}
