//! Child process supervisor for mirage-l0-sentinel.
//!
//! Manages two child slots — `cli` (auto-respawning) and `tun` (on-demand).
//! Each slot captures stdout/stderr into a 64 KB ring buffer.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::crash_report::{CrashKind, CrashReport};

// ── Constants ─────────────────────────────────────────────────────────────────

const RING_CAPACITY: usize = 64 * 1024; // 64 KB
const CLI_MAX_RESPAWNS: u32 = 5;
const CLI_RESPAWN_WINDOW: Duration = Duration::from_secs(60);

// ── RingBuffer ────────────────────────────────────────────────────────────────

/// Fixed-capacity byte ring buffer. Drops oldest bytes when full.
pub struct RingBuffer {
    buf: VecDeque<u8>,
    cap: usize,
}

impl RingBuffer {
    pub fn new(cap: usize) -> Self {
        Self { buf: VecDeque::with_capacity(cap), cap }
    }

    /// Push bytes, dropping oldest on overflow.
    pub fn push(&mut self, data: &[u8]) {
        let available = self.cap.saturating_sub(self.buf.len());
        if data.len() > available {
            let drop_count = data.len() - available;
            let actual_drop = drop_count.min(self.buf.len());
            for _ in 0..actual_drop {
                self.buf.pop_front();
            }
        }
        for &b in data {
            if self.buf.len() < self.cap {
                self.buf.push_back(b);
            }
        }
    }

    /// Return the last `n` bytes as a UTF-8 string (invalid bytes replaced).
    pub fn tail_utf8(&self, n: usize) -> String {
        let start = self.buf.len().saturating_sub(n);
        let slice: Vec<u8> = self.buf.range(start..).copied().collect();
        String::from_utf8_lossy(&slice).into_owned()
    }

    /// Return all bytes as a UTF-8 string (invalid bytes replaced).
    pub fn all_utf8(&self) -> String {
        let slice: Vec<u8> = self.buf.iter().copied().collect();
        String::from_utf8_lossy(&slice).into_owned()
    }
}

// ── ChildHandle ───────────────────────────────────────────────────────────────

/// Live handle to a running child process.
pub struct ChildHandle {
    pub process: tokio::process::Child,
    pub stdout_buf: Arc<Mutex<RingBuffer>>,
    pub stderr_buf: Arc<Mutex<RingBuffer>>,
    pub started_at: Instant,
    pub started_at_ms: u64,
}

impl ChildHandle {
    fn pid(&self) -> Option<u32> {
        self.process.id()
    }
}

// ── ChildStatus (JSON-serialisable snapshot) ──────────────────────────────────

#[derive(serde::Serialize, Clone)]
pub struct ChildStatus {
    pub alive: bool,
    pub pid: Option<u32>,
    pub respawn_count: u32,
}

// ── ChildState ────────────────────────────────────────────────────────────────

/// Supervisor state for both child slots.
pub struct ChildState {
    pub cli: Option<ChildHandle>,
    pub tun: Option<ChildHandle>,
    pub cli_respawn_count: u32,
    pub tun_respawn_count: u32,
    /// Timestamps of recent cli respawns (for rate-limiting).
    pub cli_respawn_timestamps: VecDeque<Instant>,
}

impl ChildState {
    pub fn new() -> Self {
        Self {
            cli: None,
            tun: None,
            cli_respawn_count: 0,
            tun_respawn_count: 0,
            cli_respawn_timestamps: VecDeque::new(),
        }
    }

    pub fn cli_status(&self) -> ChildStatus {
        ChildStatus {
            alive: self.cli.is_some(),
            pid: self.cli.as_ref().and_then(|h| h.pid()),
            respawn_count: self.cli_respawn_count,
        }
    }

    pub fn tun_status(&self) -> ChildStatus {
        ChildStatus {
            alive: self.tun.is_some(),
            pid: self.tun.as_ref().and_then(|h| h.pid()),
            respawn_count: self.tun_respawn_count,
        }
    }
}

// ── Spawn helpers ─────────────────────────────────────────────────────────────

/// Spawn a child process with piped stdout/stderr; start drain tasks.
pub fn spawn_child(
    binary: &PathBuf,
    args: &[String],
) -> Result<ChildHandle> {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {:?}", binary))?;

    let stdout_buf = Arc::new(Mutex::new(RingBuffer::new(RING_CAPACITY)));
    let stderr_buf = Arc::new(Mutex::new(RingBuffer::new(RING_CAPACITY)));

    // Drain stdout
    if let Some(stdout) = child.stdout.take() {
        let buf = Arc::clone(&stdout_buf);
        tokio::spawn(drain_pipe(stdout, buf));
    }

    // Drain stderr
    if let Some(stderr) = child.stderr.take() {
        let buf = Arc::clone(&stderr_buf);
        tokio::spawn(drain_pipe(stderr, buf));
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    Ok(ChildHandle {
        process: child,
        stdout_buf,
        stderr_buf,
        started_at: Instant::now(),
        started_at_ms: now_ms,
    })
}

/// Background task: read from pipe into ring buffer until EOF.
async fn drain_pipe<R: AsyncReadExt + Unpin>(mut reader: R, buf: Arc<Mutex<RingBuffer>>) {
    let mut tmp = vec![0u8; 4096];
    loop {
        match reader.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                buf.lock().await.push(&tmp[..n]);
            }
            Err(e) => {
                warn!("[supervisor] pipe read error: {}", e);
                break;
            }
        }
    }
}

// ── Cli respawn logic ─────────────────────────────────────────────────────────

/// Returns true if cli may be respawned (under rate limit).
pub fn cli_may_respawn(state: &mut ChildState) -> bool {
    let now = Instant::now();
    // Purge timestamps outside the window.
    while let Some(&front) = state.cli_respawn_timestamps.front() {
        if now.duration_since(front) > CLI_RESPAWN_WINDOW {
            state.cli_respawn_timestamps.pop_front();
        } else {
            break;
        }
    }
    state.cli_respawn_timestamps.len() < CLI_MAX_RESPAWNS as usize
}

/// Record a cli respawn attempt.
pub fn record_cli_respawn(state: &mut ChildState) {
    state.cli_respawn_timestamps.push_back(Instant::now());
    state.cli_respawn_count += 1;
}

// ── Crash capture ─────────────────────────────────────────────────────────────

/// Capture stderr/stdout tails from a handle before it's dropped.
pub async fn capture_crash_tails(
    stdout_buf: &Arc<Mutex<RingBuffer>>,
    stderr_buf: &Arc<Mutex<RingBuffer>>,
) -> (String, String) {
    let stderr_tail = stderr_buf.lock().await.tail_utf8(4096);
    let stdout_tail = stdout_buf.lock().await.tail_utf8(4096);
    (stderr_tail, stdout_tail)
}

/// Build a CrashReport from an exited child.
pub async fn build_crash_report(
    process_name: &str,
    exit_status: std::process::ExitStatus,
    handle: &ChildHandle,
) -> CrashReport {
    let (stderr_tail, stdout_tail) =
        capture_crash_tails(&handle.stdout_buf, &handle.stderr_buf).await;

    let exit_code = exit_status.code();

    #[cfg(unix)]
    let kind = {
        use std::os::unix::process::ExitStatusExt;
        if exit_status.signal().is_some() {
            CrashKind::Killed
        } else if exit_code != Some(0) {
            CrashKind::ExitNonzero
        } else {
            CrashKind::Other
        }
    };
    #[cfg(not(unix))]
    let kind = if exit_code != Some(0) {
        CrashKind::ExitNonzero
    } else {
        CrashKind::Other
    };

    let died_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let uptime_ms = handle.started_at.elapsed().as_millis() as u64;

    CrashReport {
        process: process_name.to_owned(),
        kind,
        exit_code,
        started_at_ms: handle.started_at_ms,
        died_at_ms,
        uptime_ms,
        stderr_tail,
        stdout_tail,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
    }
}

/// Kill tun child gracefully (best-effort).
pub async fn kill_tun(handle: &mut ChildHandle) {
    if let Err(e) = handle.process.kill().await {
        warn!("[supervisor] kill tun: {}", e);
    }
    match tokio::time::timeout(Duration::from_secs(5), handle.process.wait()).await {
        Ok(Ok(s)) => info!("[supervisor] tun exited: {:?}", s),
        Ok(Err(e)) => warn!("[supervisor] tun wait: {}", e),
        Err(_) => warn!("[supervisor] tun did not exit within 5s"),
    }
}

/// Resolve tun binary path: explicit config or `<exe_dir>/mirage2tun[.exe]`.
pub fn resolve_tun_binary(configured: Option<&PathBuf>) -> Option<PathBuf> {
    if let Some(p) = configured {
        return Some(p.clone());
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    #[cfg(windows)]
    let name = "mirage2tun.exe";
    #[cfg(not(windows))]
    let name = "mirage2tun";
    Some(dir.join(name))
}

/// Log a supervisor error to tracing.
pub fn log_crash(report: &CrashReport) {
    error!(
        "[supervisor] {} crashed: kind={:?} exit_code={:?} uptime_ms={} stderr={}",
        report.process,
        report.kind,
        report.exit_code,
        report.uptime_ms,
        report.stderr_tail.lines().last().unwrap_or(""),
    );
}
