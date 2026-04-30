//! PID file helpers for sentinel child-process tracking.
//!
//! Each child slot (`cli`, `tun`) writes a small text file containing just the
//! decimal PID.  On respawn the new sentinel instance reads these files and can
//! adopt the still-running children rather than killing and re-spawning them.

use std::path::{Path, PathBuf};

use tracing::{info, warn};

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Return `<exe_dir>/cli.pid` (or `None` if `current_exe` fails).
pub fn cli_pid_path() -> Option<PathBuf> {
    pid_path("cli.pid")
}

/// Return `<exe_dir>/tun.pid` (or `None` if `current_exe` fails).
pub fn tun_pid_path() -> Option<PathBuf> {
    pid_path("tun.pid")
}

fn pid_path(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    Some(dir.join(name))
}

// ── Write / delete ────────────────────────────────────────────────────────────

/// Write `pid` to `path` as a plain decimal string.
pub fn write(path: &Path, pid: u32) {
    match std::fs::write(path, pid.to_string()) {
        Ok(()) => info!("[pid_file] wrote {} → {:?}", pid, path),
        Err(e) => warn!("[pid_file] failed to write {:?}: {}", path, e),
    }
}

/// Delete the PID file.  Silently ignores "not found" errors.
pub fn remove(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => info!("[pid_file] removed {:?}", path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("[pid_file] failed to remove {:?}: {}", path, e),
    }
}

/// Read PID from file.  Returns `None` on any error (missing, parse fail, …).
pub fn read(path: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(path).ok()?;
    raw.trim().parse().ok()
}

// ── Liveness + image-name check ───────────────────────────────────────────────

/// Result of probing a PID file for a live matching child.
pub enum ProbeResult {
    /// Process is alive and its image name matches the expected substring.
    Alive { pid: u32 },
    /// File existed but process is dead or image name doesn't match — stale.
    Stale,
    /// PID file does not exist.
    Missing,
}

/// Probe a PID file.
///
/// * `path`           — path to the `.pid` file
/// * `exe_name_hint`  — substring that must appear in the process image name
///                      (e.g. `"mirage-client-cli"` or `"mirage2tun"`)
pub fn probe(path: &Path, exe_name_hint: &str) -> ProbeResult {
    let Some(pid) = read(path) else {
        return ProbeResult::Missing;
    };

    if !is_alive_with_name(pid, exe_name_hint) {
        warn!(
            "[pid_file] stale PID {} in {:?} (dead or wrong image)",
            pid, path
        );
        remove(path);
        return ProbeResult::Stale;
    }

    info!("[pid_file] adopted PID {} from {:?}", pid, path);
    ProbeResult::Alive { pid }
}

// ── Platform implementations ──────────────────────────────────────────────────

#[cfg(windows)]
fn is_alive_with_name(pid: u32, name_hint: &str) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION,
            FALSE,
            pid,
        );
        if handle.is_null() {
            return false;
        }

        let alive = check_image_name(handle, name_hint);
        CloseHandle(handle);
        alive
    }
}

#[cfg(windows)]
unsafe fn check_image_name(handle: windows_sys::Win32::Foundation::HANDLE, hint: &str) -> bool {
    use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;

    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // PROCESS_NAME_WIN32 = 0
    let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
    if ok == 0 {
        return false;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.to_lowercase().contains(&hint.to_lowercase())
}

#[cfg(not(windows))]
fn is_alive_with_name(pid: u32, name_hint: &str) -> bool {
    let exe_link = format!("/proc/{}/exe", pid);
    match std::fs::read_link(std::path::Path::new(&exe_link)) {
        Ok(p) => p.to_string_lossy().to_lowercase().contains(&name_hint.to_lowercase()),
        Err(_) => false,
    }
}
