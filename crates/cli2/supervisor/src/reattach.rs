//! Orphan worker discovery: scan PID files, probe pipes, reattach live workers.

use std::path::Path;

use mirage_chain_ipc::{
    WorkerMsg,
    pipe_path,
    pipe::{PipePair, connect},
};
use tracing::{info, warn};

/// A live worker that has been reattached.
pub struct OrphanInfo {
    pub chain_id: String,
    pub pid: u32,
    pub ipc: PipePair,
}

/// Scan `${TEMP}/mirage-cli2/` for PID files, probe the matching pipes,
/// and return info for every worker that is alive and reachable.
pub async fn discover_orphans(temp_dir: &Path) -> Vec<OrphanInfo> {
    let cli2_dir = temp_dir.join("mirage-cli2");
    let mut result = Vec::new();

    let entries = match std::fs::read_dir(&cli2_dir) {
        Ok(e) => e,
        Err(e) => {
            info!("no orphan directory ({}): {e}", cli2_dir.display());
            return result;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let fname = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Only look at .pid files.
        let Some(chain_id) = fname.strip_suffix(".pid") else {
            continue;
        };
        let chain_id = chain_id.to_string();

        // Read PID.
        let pid_str = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                warn!(chain_id = %chain_id, "could not read PID file: {e}");
                continue;
            }
        };
        let pid: u32 = match pid_str.trim().parse() {
            Ok(p) => p,
            Err(e) => {
                warn!(chain_id = %chain_id, "invalid PID in file: {e}");
                continue;
            }
        };

        // Check process is alive.
        if !process_alive(pid) {
            info!(chain_id = %chain_id, pid, "stale PID file — process dead, skipping");
            // Remove stale PID file so it doesn't confuse the next run.
            let _ = std::fs::remove_file(&path);
            continue;
        }

        // Probe the pipe.
        let pipe_addr = pipe_path(&chain_id);
        match connect(&pipe_addr).await {
            Ok(mut ipc) => {
                // Worker should send Hello immediately after we connect.
                match ipc.read_msg::<WorkerMsg>().await {
                    Ok(Some(WorkerMsg::Hello { chain_id: ref cid, pid: wpid, .. })) => {
                        info!(
                            chain_id = %cid,
                            pid = wpid,
                            "reattached to orphan worker"
                        );
                        result.push(OrphanInfo {
                            chain_id: chain_id.clone(),
                            pid: wpid,
                            ipc,
                        });
                    }
                    Ok(Some(other)) => {
                        warn!(
                            chain_id = %chain_id,
                            ?other,
                            "unexpected first message from orphan — skipping"
                        );
                    }
                    Ok(None) => {
                        warn!(chain_id = %chain_id, "orphan pipe EOF on first message");
                    }
                    Err(e) => {
                        warn!(chain_id = %chain_id, "orphan IPC read error: {e}");
                    }
                }
            }
            Err(e) => {
                info!(chain_id = %chain_id, "could not connect to orphan pipe: {e}");
            }
        }
    }

    result
}

/// Returns `true` if a process with `pid` is running on this platform.
fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        windows_process_alive(pid)
    }
    #[cfg(not(windows))]
    {
        unix_process_alive(pid)
    }
}

#[cfg(windows)]
fn windows_process_alive(pid: u32) -> bool {
    // SAFETY: OpenProcess with SYNCHRONIZE is a standard liveness check.
    // The handle is closed immediately.
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};

    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    unsafe { CloseHandle(handle) };
    true
}

#[cfg(not(windows))]
fn unix_process_alive(pid: u32) -> bool {
    // kill(pid, 0) succeeds if the process exists and we have permission.
    // SAFETY: we pass signal 0 which never kills.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    ret == 0
}
