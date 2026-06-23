// GUI subsystem on Windows for ALL profiles — no console window ever. All logs
// go to `mirage-launcher.log` next to the exe, so the debug console is just noise.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::f64::consts::TAU;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod dns_resolver;
mod mesh_state;
mod model;
mod sentinel_status;

use dns_resolver::DnsResolver;
use mesh_state::{HopRole, MeshSnapshot};
use sentinel_status::SentinelPhaseLocal;

#[cfg(windows)]
mod jobobj {
    //! Wrap launcher + spawned children in a Windows Job Object with
    //! KILL_ON_JOB_CLOSE — any exit of the launcher cascades a forced kill
    //! to every child, even if our shutdown code never runs.

    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;

    struct JobHandle { _raw: HANDLE }
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    static JOB: OnceLock<JobHandle> = OnceLock::new();

    pub fn init() {
        // Disabled in v2: a self-job with KILL_ON_JOB_CLOSE risks killing the
        // launcher itself when any child wraps badly.  Children are tracked by
        // PID + watchdog respawn instead; on launcher close we kill them
        // explicitly in the WindowEvent::CloseRequested handler.
        let _ = JOB;
    }
}

use softbuffer::Surface;
use uzor::input::{InputState, MouseButton as UzorMouseButton};
use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor_render_tiny_skia::TinySkiaCpuRenderContext;
use uzor_window_desktop::winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Icon, Window, WindowId},
};

/// Bundled 64×64 PNG icon — rendered at build time by `assets/gen_icon.py`.
/// Used as the runtime window icon (loaded via `winit::window::Icon`).  The
/// .exe icon comes from `build.rs` embedding `assets/icon.ico` directly.
const ICON_PNG: &[u8] = include_bytes!("../assets/icon-64.png");

// ── Theme (taken from ClientApp.jsx CLIENT_S palette) ────────────────────────

const BG: &str = "#0B0B0E";
/// Dot-grid color.
const DOT: &str = "#ffffff38"; // rgba(255,255,255,0.22)
/// Dot-grid spacing (logical px).
const DOT_STEP: f64 = 26.0;
const SURFACE: &str = "#14151A";
const BORDERS: &str = "#ffffff24"; // rgba(255,255,255,0.14)
const FG0: &str = "#F4F4F5";
const FG1: &str = "#878B91"; // muted mid-tone (brand)
const FG3: &str = "#555860";
const ACCENT: &str = "#FBB26A";

/// White cube color — used for Unknown/Starting/FetchingConfig/ConfigReady phases.
const CUBE_WHITE: &str = "#F4F4F5";
/// Amber cube color — ChildRunning phase.
const CUBE_AMBER: &str = "#FBB26A";
/// Red cube color — Error phase.
const CUBE_RED: &str = "#E05656";

const W: f64 = 380.0;
const H: f64 = 540.0;

/// Custom-chrome titlebar height (logical pixels).  Top of the window is a
/// drag region except for the close/minimise buttons on the right.
const TITLEBAR_H: f64 = 36.0;
const TITLEBAR_BTN_W: f64 = 44.0;

/// Sentinel HTTP base URL.
const SENTINEL_BASE: &str = "http://127.0.0.1:19420";

// ── Launcher state ───────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum LauncherState {
    Off,
    /// `started_at_ms` — animation time when Connecting started, drives both
    /// the cube dissolve and the doppler radar phase so the first ring is
    /// always seen leaving the centre at the moment of the click.
    Connecting { started_at_ms: f64 },
    /// `radar_anchor_ms` — animation time the radar was anchored at; carried
    /// forward from Connecting so the doppler doesn't reset its phase when we
    /// transition from Connecting → On.
    On { radar_anchor_ms: f64 },
    Error(String),
}

enum LauncherEvent {
    Started,
    Failed(String),
    Stopped,
}

// ── Child processes ──────────────────────────────────────────────────────────
//
// Sentinel supervises cli and tun internally.  Launcher holds only a handle
// to sentinel so it can kill it (and thereby the entire child tree via the
// Job Object) on window close.

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .expect("current_exe always valid")
        .parent()
        .expect("exe always has parent")
        .to_path_buf()
}

/// Windows CREATE_NO_WINDOW — prevents the spawned child from getting its own
/// console window when our GUI launcher spawns CLI binaries.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Write a default `sentinel.toml` if it does not already exist.
///
/// The generated file sets `spawn_cli = false` so the orchestrator (not
/// sentinel) owns the cli lifecycle when launched via `ensure_orchestrator_toml`.
fn ensure_sentinel_toml() {
    let dir = exe_dir();
    let toml_path = dir.join("sentinel.toml");
    if toml_path.exists() {
        return;
    }

    let dir_str = dir.to_string_lossy().replace('\\', "/");
    let client_config = format!("{dir_str}/client-config.json");
    let exe = std::env::consts::EXE_SUFFIX;
    let tun_bin = format!("{dir_str}/mirage2tun{exe}");

    let content = format!(
        r#"# Auto-generated by mirage-launcher.  Replace fields after onboarding.

monitor_url = "http://127.0.0.1:19410"
# monitor_api_key = "..."   # populated by onboarding QR code in the future

# Where to materialise the merged ClientConfig.json that cli will read.
client_config_path = "{client_config}"

# Where the existing client-config.json template lives — sentinel uses this
# as the base and overlays chains[] from monitor.
client_template = "{client_config}"

tun_binary = "{tun_bin}"
tun_args   = ["debug-run"]

poll_interval_secs = 30
respect_user_config = true

# Orchestrator owns the cli — sentinel must not also spawn it.
spawn_cli = false
"#
    );

    if let Err(e) = std::fs::write(&toml_path, content) {
        tracing::warn!("could not write sentinel.toml: {}", e);
    }
}

/// Write a default `orchestrator.toml` if it does not already exist.
///
/// Describes two process-mode sibling components:
/// - `sentinel`: bootstraps identity + materialises client-config.json.
/// - `data-plane`: the cli2 `mirage-chain-supervisor` (OTA-safe detached
///   workers). It waits for client-config.json and reads it as `argv[1]`.
///
/// cli1 was retired 2026-06-14 — cli2 is the only data-plane.
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

    // cli2-supervisor takes the config path positionally (`argv[1]`) and reads
    // the unified ClientConfig JSON via `load_any`; it gates on client-config.json
    // via wait_for_file.
    let content = format!(
        "# Auto-generated by mirage-launcher.\n\
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
        tracing::warn!("could not write orchestrator.toml: {}", e);
    }
}

/// Write a minimal `client-config.json` template carrying the `supervisor` block
/// (worker_binary, reload poll) when none exists yet. The sentinel overlays
/// monitor-pushed `chains[]` onto this template; `materialise` mutates only
/// `chains`/creds/`_managed_by`, so the `supervisor` block survives and
/// cli2-supervisor can lower it via `from_client_config`. Skipped if a config
/// already exists (installer- or sentinel-managed).
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
        tracing::warn!("could not write client-config.json cli2 template: {}", e);
    }
}

/// Spawn `mirage-l0-orchestrator.exe` from `exe_dir()` with `--config orchestrator.toml`.
/// The orchestrator in turn manages sentinel and cli as sibling child processes.
fn reap_orphans() {
    use std::process::Command;
    #[cfg(unix)]
    {
        // Best-effort: kill any leftover sentinel/cli from a previous (crashed)
        // launcher run.  These are user-owned, so plain pkill works.  Tun runs
        // as root and is killed via sudo (already cached if user just OK'd it
        // this session; harmless prompt if not).
        // cli1 (mirage-client-cli) retired 2026-06-14; the cli2 data-plane is
        // supervisor + workers — reap those too so a crashed run's orphans don't
        // hold the SOCKS/utun ports (AddrInUse on the next connect).
        for name in &[
            "mirage-l0-sentinel",
            "mirage-chain-supervisor",
            "mirage-chain-worker",
        ] {
            let _ = Command::new("pkill").arg("-9").arg("-x").arg(name).status();
        }
        // Tun (root) — try sudo -n (cached), don't prompt; if no cache, skip.
        let _ = Command::new("sudo").args(["-n", "pkill", "-9", "-x", "mirage2tun"]).status();
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
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
    // Brief sleep so OS releases ports/utun before next spawn.
    std::thread::sleep(std::time::Duration::from_millis(300));
}

fn spawn_orchestrator() -> anyhow::Result<std::process::Child> {
    // std::process::Command, not tokio: this fn is called before the tokio
    // runtime is entered (Init handler), and tokio::process requires an
    // active reactor.  We don't need to async-await this child anyway —
    // we only kill it on shutdown.
    //
    // Reap orphans first.  On macOS our tun runs as root via osascript, so
    // it is NOT in our process group — KILL_ON_JOB_CLOSE (Win) has no
    // analogue.  If a previous launcher crashed, the old cli/tun keep their
    // ports/utun and the fresh cli would hit AddrInUse.  pkill clears that.
    reap_orphans();

    let dir = exe_dir();
    let config = dir.join("orchestrator.toml");

    // All child stdout/stderr → unified mirage-launcher.log (single source of
    // truth; testers send only one file).
    let log_path = dir.join("mirage-launcher.log");
    let log = std::fs::OpenOptions::new()
        .create(true).append(true)
        .open(&log_path)
        .map_err(|e| anyhow::anyhow!("open {:?}: {e}", log_path))?;
    let log2 = log.try_clone().map_err(|e| anyhow::anyhow!("clone log: {e}"))?;

    #[cfg(windows)]
    let child = {
        use std::os::windows::process::CommandExt;
        let orch_exe = dir.join("mirage-l0-orchestrator.exe");
        std::process::Command::new(&orch_exe)
            .arg("--config").arg(&config)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?
    };
    #[cfg(not(windows))]
    let child = {
        let orch_exe = dir.join("mirage-l0-orchestrator");
        std::process::Command::new(&orch_exe)
            .arg("--config").arg(&config)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2))
            .spawn()?
    };

    Ok(child)
}

// ── Mouse accumulator ────────────────────────────────────────────────────────

#[derive(Default)]
struct MouseAccum {
    pos: Option<(f64, f64)>,
    button_down: Option<UzorMouseButton>,
    clicked: Option<UzorMouseButton>,
}

// ── App state ────────────────────────────────────────────────────────────────

struct LauncherApp {
    window: Arc<Window>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    mouse: MouseAccum,
    last_frame: Instant,
    startup: Instant,
    state: LauncherState,
    /// Handle to the sentinel child process.
    sentinel: Arc<Mutex<Option<std::process::Child>>>,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<LauncherEvent>,
    event_tx: tokio::sync::mpsc::UnboundedSender<LauncherEvent>,
    rt: Arc<tokio::runtime::Runtime>,
    coordinator: uzor::InputCoordinator,
    /// Live per-chain stats from `mirage2tun /metrics` — `None` until the
    /// poller has produced its first snapshot.
    mesh_rx: tokio::sync::watch::Receiver<MeshSnapshot>,
    /// Phase reported by the sentinel process.
    sentinel_rx: tokio::sync::watch::Receiver<SentinelPhaseLocal>,
    /// Reverse-DNS resolver — kept alive to maintain the background worker.
    _dns_resolver: Arc<DnsResolver>,
    /// Timestamp (ms) when we entered the current sentinel phase — used to
    /// drive fade-in animations.
    sentinel_phase_since_ms: f64,
    /// Previous sentinel phase — used to detect transitions.
    sentinel_phase_prev: SentinelPhaseLocal,
    /// Unified client-state model — updated every ~500 ms by `model::spawn_model_poller`.
    /// The visual layer reads `.borrow()` when it needs a rich snapshot.
    /// Prefixed `_` because no draw code consumes it yet; field kept so the
    /// poller task stays alive for the lifetime of the app.
    _model_rx: tokio::sync::watch::Receiver<std::sync::Arc<model::ClientState>>,
    /// Whether the slide-down menu overlay is open (toggled by the top-left
    /// "M" / parked-cube button).
    menu_open: bool,
}

struct LauncherHandler {
    app: Option<LauncherApp>,
    rt: Arc<tokio::runtime::Runtime>,
    /// mirage://... URL passed as a CLI argument — forwarded to sentinel
    /// after it becomes reachable.
    cli_onboard_url: Option<String>,
}

// ── Rectangular mesh layout ──────────────────────────────────────────────────
//
// Replaces the radial wheel.  Nodes are snapped to DOT_STEP (26 px) grid.
// Layout:
//   Destinations  → rows starting at cy - 6*DOT_STEP and above
//   L2 / Exit     → row at cy - 4*DOT_STEP
//   L1 / Entry    → row at cy - 2*DOT_STEP
//   Centre (user) → (cx, cy)
//
// Lines go straight up from centre → L1 row → L2 row → dest row(s).
// Chains sharing a node use the same grid slot (different colour + ±2px offset).

/// Activity threshold — chains with `bytes_per_sec` below this show as dim
/// (idle) but still rendered.  Above it they pulse and trail.
const ACTIVE_BYTES_PER_SEC: f64 = 256.0;
/// Recency threshold — a chain is "active" if any traffic was seen within
/// this window, regardless of the current poll's instantaneous rate.
const ACTIVE_WINDOW_MS: f64 = 3000.0;
/// Recency threshold — chains last active longer ago than this fade out.
const STALE_AFTER_MS: f64 = 8000.0;

fn chain_color(id: &str) -> &'static str {
    let mut h: u32 = 5381;
    for b in id.bytes() { h = h.wrapping_mul(33).wrapping_add(b as u32); }
    match h % 5 {
        0 => "#FBB26A",
        1 => "#FF9866",
        2 => "#E8A838",
        3 => "#6DB8F5",
        _ => "#FBC890",
    }
}

/// Snap a logical (x,y) point onto the nearest dot-grid intersection.
fn snap_to_dot(x: f64, y: f64) -> (f64, f64) {
    let nx = ((x / DOT_STEP).round() * DOT_STEP).clamp(12.0, W - 12.0);
    let ny = ((y / DOT_STEP).round() * DOT_STEP).clamp(TITLEBAR_H + 12.0, H - 12.0);
    (nx, ny)
}

/// Spread `n` items evenly across window width, returning their x-coordinates.
/// Margins: 32px on each side.
fn draw_mesh_wheel(
    ctx: &mut TinySkiaCpuRenderContext,
    cx: f64,
    cy: f64,
    t: f64,
    t_local: f64,
    snapshot: &MeshSnapshot,
) {
    use std::collections::HashMap;

    // Radar removed (owner: too jarring — to be redesigned later).
    let _ = t_local;

    // Filter synthetic chains.
    let chains: Vec<&mesh_state::ChainView> = snapshot.chains.iter()
        .filter(|c| !c.id.starts_with("__"))
        .collect();

    let n_chains = chains.len();

    // ── Build unique node table keyed by addr ────────────────────────────────
    //
    // Each unique addr gets a grid position based on its role row.
    // Within a role row, nodes are spread horizontally in the order they first
    // appear.

    // Collect unique addrs per role, preserving insertion order.
    let mut entry_addrs: Vec<String> = Vec::new();
    let mut exit_addrs: Vec<String> = Vec::new();
    let mut middle_addrs: Vec<String> = Vec::new();

    for c in &chains {
        for h in &c.hops {
            let list = match h.role {
                HopRole::Entry => &mut entry_addrs,
                HopRole::Exit => &mut exit_addrs,
                HopRole::Middle => &mut middle_addrs,
            };
            if !list.contains(&h.addr) {
                list.push(h.addr.clone());
            }
        }
    }

    // Grid rows for each role (counting from cy upward).
    // Entry (L1) at 2 steps up, Middle (extra hops) at 3 steps up, Exit (L2) at 4 steps up.
    let row_y_entry = cy - 2.0 * DOT_STEP;
    let row_y_middle = cy - 3.0 * DOT_STEP;
    let row_y_exit = cy - 4.0 * DOT_STEP;

    let node_x = |addrs: &[String], idx: usize| -> f64 {
        let n = addrs.len();
        if n == 0 { return cx; }
        let margin = 32.0;
        let usable = W - margin * 2.0;
        if n == 1 {
            W / 2.0
        } else {
            let step = usable / (n - 1) as f64;
            margin + idx as f64 * step
        }
    };

    // Build node position map.
    let mut node_pos: HashMap<String, (f64, f64)> = HashMap::new();
    for (i, addr) in entry_addrs.iter().enumerate() {
        let raw = (node_x(&entry_addrs, i), row_y_entry);
        node_pos.insert(addr.clone(), snap_to_dot(raw.0, raw.1));
    }
    for (i, addr) in middle_addrs.iter().enumerate() {
        let raw = (node_x(&middle_addrs, i), row_y_middle);
        node_pos.insert(addr.clone(), snap_to_dot(raw.0, raw.1));
    }
    for (i, addr) in exit_addrs.iter().enumerate() {
        let raw = (node_x(&exit_addrs, i), row_y_exit);
        node_pos.insert(addr.clone(), snap_to_dot(raw.0, raw.1));
    }

    // ── Destination rows ─────────────────────────────────────────────────────
    //
    // Collect all unique destination IPs across chains; place them in rows
    // starting at cy - 6*DOT_STEP.  Each dest gets a slot.

    // Collect dest ips — we want stable ordering, so sort by (chain_idx, dest_idx).
    let mut all_dests: Vec<(String, Option<String>)> = Vec::new(); // (ip_port_key, domain)
    for c in &chains {
        for d in &c.destinations {
            let key = format!("{}:{}", d.ip, d.port);
            if !all_dests.iter().any(|(k, _)| k == &key) {
                all_dests.push((key, d.domain.clone()));
            }
        }
    }

    const DEST_COLS: usize = 8;
    let dest_row_base_y = cy - 6.0 * DOT_STEP;

    let dest_pos: HashMap<String, (f64, f64)> = {
        let n = all_dests.len();
        let mut map = HashMap::new();
        for (i, (key, _)) in all_dests.iter().enumerate() {
            let col = i % DEST_COLS;
            let row_idx = i / DEST_COLS;
            let row_y = dest_row_base_y - row_idx as f64 * DOT_STEP;
            let margin = 16.0;
            let usable = W - margin * 2.0;
            let cols = DEST_COLS.min(n);
            let x = if cols <= 1 {
                W / 2.0
            } else {
                margin + col as f64 * usable / (cols - 1).max(1) as f64
            };
            map.insert(key.clone(), snap_to_dot(x, row_y));
        }
        map
    };

    // ── Draw chain lines ─────────────────────────────────────────────────────
    for (i, c) in chains.iter().enumerate() {
        let active = c.bytes_per_sec >= ACTIVE_BYTES_PER_SEC
            || c.last_active_ms < ACTIVE_WINDOW_MS;
        let stale = c.last_active_ms > STALE_AFTER_MS;
        let alpha = if stale { 0.20 } else if active { 1.0 } else { 0.55 };
        let color = chain_color(&c.id);

        // Chain offset for shared-node visual separation.
        let offset_x = if n_chains > 1 {
            let slot = i as f64 / (n_chains - 1).max(1) as f64 - 0.5;
            slot * 4.0
        } else {
            0.0
        };

        // Build hop path: centre → entry hops → middle hops → exit hops.
        let mut path: Vec<(f64, f64)> = vec![(cx + offset_x, cy)];
        for h in &c.hops {
            if let Some(&pos) = node_pos.get(&h.addr) {
                path.push((pos.0 + offset_x, pos.1));
            }
        }

        let line_w = if active {
            (1.0 + c.bytes_per_sec.log10().max(0.0) * 0.5).min(3.5)
        } else { 0.8 };

        ctx.set_global_alpha(alpha);
        ctx.set_stroke_color(color);
        ctx.set_stroke_width(line_w);
        if !active { ctx.set_line_dash(&[2.0, 3.0]); }
        ctx.begin_path();
        ctx.move_to(path[0].0, path[0].1);
        for &p in &path[1..] { ctx.line_to(p.0, p.1); }
        ctx.stroke();
        ctx.set_line_dash(&[]);
        ctx.set_global_alpha(1.0);

        // Travelling pulse along the trunk.
        if active && path.len() >= 2 {
            let segments = path.len() - 1;
            let speed = (c.bytes_per_sec / 5_000.0).clamp(0.5, 4.0);
            let phase = ((t * speed / 2_000.0) + i as f64 * 0.13).rem_euclid(1.0);
            let scaled = phase * segments as f64;
            let seg_idx = (scaled.floor() as usize).min(segments - 1);
            let local = scaled - seg_idx as f64;
            let a = path[seg_idx];
            let b = path[seg_idx + 1];
            let px = a.0 + (b.0 - a.0) * local;
            let py = a.1 + (b.1 - a.1) * local;
            ctx.set_global_alpha(0.85);
            ctx.set_fill_color(color);
            ctx.begin_path();
            ctx.arc(px, py, 2.5, 0.0, TAU);
            ctx.fill();
            ctx.set_global_alpha(1.0);
        }

        // ── Destination lines from exit node (or centre if no hops) ─────────
        let exit_pos = *path.last().unwrap_or(&(cx, cy));
        let max_bytes = c.destinations.iter().map(|d| d.bytes).max().unwrap_or(1).max(1) as f64;
        for d in &c.destinations {
            let key = format!("{}:{}", d.ip, d.port);
            let &dest_pos_pt = match dest_pos.get(&key) {
                Some(p) => p,
                None => continue,
            };

            let weight = (d.bytes as f64 / max_bytes).powf(0.5).clamp(0.10, 1.0);
            let dest_alpha = alpha * (0.30 + 0.70 * weight);

            let style = if d.bytes >= 256 * 1024 { 2 }
                else if d.bytes >= 16 * 1024 { 1 }
                else { 0 };
            let line_w = match style { 2 => 1.6, 1 => 1.0, _ => 0.6 };
            let dash: &[f64] = match style { 2 => &[], 1 => &[3.0, 3.0], _ => &[1.0, 3.0] };

            ctx.set_global_alpha(dest_alpha);
            ctx.set_stroke_color(color);
            ctx.set_stroke_width(line_w);
            ctx.set_line_dash(dash);
            ctx.begin_path();
            ctx.move_to(exit_pos.0, exit_pos.1);
            ctx.line_to(dest_pos_pt.0, dest_pos_pt.1);
            ctx.stroke();
            ctx.set_line_dash(&[]);

            let dot_r = match style { 2 => 3.5, 1 => 2.5, _ => 1.8 };
            ctx.set_fill_color(color);
            ctx.begin_path();
            ctx.arc(dest_pos_pt.0, dest_pos_pt.1, dot_r, 0.0, TAU);
            ctx.fill();

            // Label only when domain is known.
            if let Some(ref domain) = d.domain {
                let trimmed: String = domain.chars().take(22).collect();
                ctx.set_fill_color(FG0);
                ctx.set_font("9px jetbrains mono");
                ctx.set_text_align(TextAlign::Center);
                ctx.set_text_baseline(TextBaseline::Top);
                ctx.fill_text(&trimmed, dest_pos_pt.0, dest_pos_pt.1 + dot_r + 4.0);
            }
            ctx.set_global_alpha(1.0);
        }
    }

    // ── Draw nodes ───────────────────────────────────────────────────────────
    //
    // Build a merged node info map for rendering (active flag, role, display name).
    let mut node_active: HashMap<String, bool> = HashMap::new();
    let mut node_display: HashMap<String, Option<String>> = HashMap::new();
    let mut node_role: HashMap<String, HopRole> = HashMap::new();
    for c in &chains {
        let active = c.bytes_per_sec >= ACTIVE_BYTES_PER_SEC || c.last_active_ms < ACTIVE_WINDOW_MS;
        for h in &c.hops {
            node_active.entry(h.addr.clone()).and_modify(|a| *a |= active).or_insert(active);
            node_display.entry(h.addr.clone()).or_insert_with(|| h.display_name.clone());
            node_role.entry(h.addr.clone()).or_insert(h.role);
        }
    }

    for (addr, &pos) in &node_pos {
        let active = *node_active.get(addr).unwrap_or(&false);
        let role = *node_role.get(addr).unwrap_or(&HopRole::Middle);
        let alpha = if active { 1.0 } else { 0.55 };
        let stroke = if active { ACCENT } else { BORDERS };
        let dot = if active { ACCENT } else { FG3 };

        ctx.set_global_alpha(alpha);

        match role {
            HopRole::Exit => {
                if active {
                    ctx.set_global_alpha(0.45);
                    ctx.set_stroke_color(ACCENT);
                    ctx.set_stroke_width(1.0);
                    ctx.begin_path();
                    ctx.arc(pos.0, pos.1, 14.0, 0.0, TAU);
                    ctx.stroke();
                    ctx.set_global_alpha(alpha);
                }
                ctx.set_fill_color(SURFACE);
                ctx.begin_path();
                ctx.arc(pos.0, pos.1, 10.0, 0.0, TAU);
                ctx.fill();
                ctx.set_stroke_color(stroke);
                ctx.set_stroke_width(if active { 1.6 } else { 1.0 });
                ctx.begin_path();
                ctx.arc(pos.0, pos.1, 10.0, 0.0, TAU);
                ctx.stroke();
                ctx.set_fill_color(dot);
                ctx.begin_path();
                ctx.arc(pos.0, pos.1, 2.5, 0.0, TAU);
                ctx.fill();
            }
            HopRole::Entry | HopRole::Middle => {
                ctx.set_fill_color(SURFACE);
                ctx.fill_rect(pos.0 - 7.0, pos.1 - 7.0, 14.0, 14.0);
                ctx.set_stroke_color(stroke);
                ctx.set_stroke_width(if active { 1.6 } else { 1.0 });
                ctx.stroke_rect(pos.0 - 7.0, pos.1 - 7.0, 14.0, 14.0);
                ctx.set_fill_color(dot);
                ctx.begin_path();
                ctx.arc(pos.0, pos.1, 2.0, 0.0, TAU);
                ctx.fill();
            }
        }

        // Node label — placed to the SIDE of the node (toward the centre, where
        // there's room) instead of underneath where it collided with the
        // destination lines. Brighter than before; tied to the node position.
        let label = node_display.get(addr)
            .and_then(|d| d.as_deref())
            .unwrap_or(addr.as_str());
        let trimmed: String = label.chars().take(18).collect();
        let r = match role { HopRole::Exit => 11.0, _ => 8.0 };
        // Left-half node → label on its right; right-half → on its left.
        let (lx, lalign) = if pos.0 <= W * 0.5 {
            (pos.0 + r + 5.0, TextAlign::Left)
        } else {
            (pos.0 - r - 5.0, TextAlign::Right)
        };
        ctx.set_global_alpha(if active { 1.0 } else { 0.65 });
        ctx.set_fill_color(if active { FG0 } else { FG1 });
        ctx.set_font("9px jetbrains mono");
        ctx.set_text_align(lalign);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(&trimmed, lx, pos.1);

        ctx.set_global_alpha(1.0);
    }

    // ── Centre user dot ──────────────────────────────────────────────────────
    ctx.set_fill_color(ACCENT);
    ctx.begin_path();
    ctx.arc(cx, cy, 6.0, 0.0, TAU);
    ctx.fill();
}

// ── Big OFF button (180px circle) ────────────────────────────────────────────

/// Doppler-style outgoing pulses — 3 rings cycling continuously.
fn draw_doppler(
    ctx: &mut TinySkiaCpuRenderContext,
    cx: f64,
    cy: f64,
    r_max: f64,
    t_local: f64,
    color: &str,
) {
    const CYCLE: f64 = 4000.0;
    const SPAWN: f64 = 1300.0;
    const OFFSETS: [f64; 3] = [0.0, SPAWN, SPAWN * 2.0];

    for offset in OFFSETS {
        let since = t_local - offset;
        if since < 0.0 { continue; }
        let local = since.rem_euclid(CYCLE);
        let phase = local / CYCLE;
        let eased = 1.0 - (1.0 - phase).powi(3);
        let r = r_max * eased;
        let alpha = 1.0 - phase;

        for (ring_offset, w_mul, a_mul) in [(0.0, 1.0, 1.0), (1.5, 2.2, 0.35), (3.0, 3.5, 0.15)] {
            ctx.set_global_alpha(alpha * a_mul);
            ctx.set_stroke_color(color);
            ctx.set_stroke_width(1.2 * w_mul);
            ctx.begin_path();
            ctx.arc(cx, cy, r + ring_offset, 0.0, TAU);
            ctx.stroke();
        }
    }
    ctx.set_global_alpha(1.0);
}

/// Off-state cube target render size (logical px).
const CUBE_OFF_SIZE: f64 = 120.0;
/// Connecting/On — outer doppler radius (covers the full window diagonally).
const FULL_RADAR_R: f64 = 360.0;
/// Time it takes for the OFF cube to dissolve after the user taps it.
const CUBE_DISSOLVE_MS: f64 = 0.0;

/// Render the cube with a heat-haze + double-exposure phantom drift.
///
/// `color` — hex color string for the cube stroke.
/// `draw_inner` — if false, only the outer hexagon is drawn (no Y/vertical lines).
/// `alpha` — scales every layer (used for dissolve and fade-in animations).
/// `shimmer` — if true, adds the heat-haze jitter and phantom copies.
fn draw_cube_idle(
    ctx: &mut TinySkiaCpuRenderContext,
    cx: f64,
    cy: f64,
    size: f64,
    t: f64,
    color: &str,
    draw_inner: bool,
    alpha: f64,
    shimmer: bool,
) {
    if shimmer {
        // Phantom A — drifts +X, slightly faded.
        let drift = (t * std::f64::consts::PI / 1000.0).sin() * 3.0;
        let phantom_a = alpha * 0.35;
        ctx.set_global_alpha(phantom_a);
        draw_cube(ctx, cx + drift, cy, size, color, draw_inner, phantom_a);

        // Phantom B — drifts -X with phase offset.
        let drift2 = ((t + 500.0) * std::f64::consts::PI / 1000.0).sin() * 3.0;
        let phantom_b = alpha * 0.25;
        ctx.set_global_alpha(phantom_b);
        draw_cube(ctx, cx - drift2, cy, size, color, draw_inner, phantom_b);

        // Main cube — small shimmer jitter.
        let jx = ((t * 0.013).sin() + (t * 0.019).cos()) * 0.6;
        let jy = ((t * 0.011).cos() + (t * 0.017).sin()) * 0.6;
        ctx.set_global_alpha(alpha);
        draw_cube(ctx, cx + jx, cy + jy, size, color, draw_inner, alpha);
    } else {
        ctx.set_global_alpha(alpha);
        draw_cube(ctx, cx, cy, size, color, draw_inner, alpha);
    }

    ctx.set_global_alpha(1.0);
}

fn draw_big_button(
    ctx: &mut TinySkiaCpuRenderContext,
    cx: f64,
    cy: f64,
    state: &LauncherState,
    t: f64,
    sentinel_phase: &SentinelPhaseLocal,
    sentinel_phase_since_ms: f64,
) {
    // WaitingOnboard — handled separately: white outer-only, dim, with prompt text.
    if matches!(sentinel_phase, SentinelPhaseLocal::WaitingOnboard) {
        draw_cube_idle(ctx, cx, cy, CUBE_OFF_SIZE, t, CUBE_WHITE, false, 0.7, false);
        ctx.set_fill_color(FG3);
        ctx.set_font("9px jetbrains mono");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("PASTE ONBOARDING LINK", cx, cy + CUBE_OFF_SIZE / 2.0 + 14.0);
        return;
    }

    // Determine cube color and which lines to draw based on sentinel phase.
    let (cube_color, draw_inner, shimmer) = match sentinel_phase {
        SentinelPhaseLocal::Unknown
        | SentinelPhaseLocal::Starting
        | SentinelPhaseLocal::FetchingConfig => (CUBE_WHITE, false, false),
        SentinelPhaseLocal::ConfigReady => {
            // Inner lines fade in over 500ms.
            (CUBE_WHITE, true, false)
        }
        SentinelPhaseLocal::ChildRunning => (CUBE_AMBER, true, true),
        // ConfigStale: amber + shimmer (same as ChildRunning).
        SentinelPhaseLocal::ConfigStale => (CUBE_AMBER, true, true),
        SentinelPhaseLocal::Error(_) => (CUBE_RED, false, false),
        // WaitingOnboard is handled above.
        SentinelPhaseLocal::WaitingOnboard => (CUBE_WHITE, false, false),
    };

    // For ConfigReady — compute inner alpha fade-in.
    let inner_alpha = match sentinel_phase {
        SentinelPhaseLocal::ConfigReady => {
            let elapsed = (t - sentinel_phase_since_ms).max(0.0);
            (elapsed / 500.0).clamp(0.0, 1.0)
        }
        SentinelPhaseLocal::ChildRunning | SentinelPhaseLocal::ConfigStale => {
            // Fade from white to amber over 600ms.
            let elapsed = (t - sentinel_phase_since_ms).max(0.0);
            (elapsed / 600.0).clamp(0.0, 1.0)
        }
        _ => 1.0,
    };

    // For ChildRunning / ConfigStale, blend cube color from white to amber.
    let effective_color = if matches!(
        sentinel_phase,
        SentinelPhaseLocal::ChildRunning | SentinelPhaseLocal::ConfigStale
    ) {
        if inner_alpha >= 1.0 { CUBE_AMBER } else { CUBE_WHITE }
    } else {
        cube_color
    };

    match state {
        LauncherState::Off => {
            draw_cube_idle(ctx, cx, cy, CUBE_OFF_SIZE, t, effective_color, draw_inner, 1.0, shimmer);
        }
        LauncherState::Error(_) => {
            draw_cube_idle(ctx, cx, cy, CUBE_OFF_SIZE, t, CUBE_RED, draw_inner, 0.7, false);
        }
        LauncherState::Connecting { .. } => {
            // Radar removed (owner: jarring). Keep the cube dancing while connecting.
            draw_cube_idle(ctx, cx, cy, CUBE_OFF_SIZE, t, effective_color, draw_inner, 1.0, shimmer);
        }
        LauncherState::On { .. } => unreachable!(),
    }

    // ConfigStale: subtle warning dot in top-right corner of cube bounding box.
    if matches!(sentinel_phase, SentinelPhaseLocal::ConfigStale) {
        let dot_x = cx + CUBE_OFF_SIZE * 0.42;
        let dot_y = cy - CUBE_OFF_SIZE * 0.42;
        ctx.set_global_alpha(0.65);
        ctx.set_fill_color(CUBE_AMBER);
        ctx.begin_path();
        ctx.arc(dot_x, dot_y, 4.0, 0.0, TAU);
        ctx.fill();
        // Exclamation mark inside the dot.
        ctx.set_fill_color(BG);
        ctx.set_font("6px jetbrains mono");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("!", dot_x, dot_y);
        ctx.set_global_alpha(1.0);
    }
}

// ── Frame compositor ─────────────────────────────────────────────────────────

/// Mirage perforated dot-grid (08 sparse — 36px spacing, α22).
fn draw_dot_grid(ctx: &mut TinySkiaCpuRenderContext) {
    ctx.set_fill_color(DOT);
    let mut y = 0.0_f64;
    while y < H {
        let mut x = 0.0_f64;
        while x < W {
            ctx.begin_path();
            ctx.arc(x, y, 1.1, 0.0, TAU);
            ctx.fill();
            x += DOT_STEP;
        }
        y += DOT_STEP;
    }
}

fn draw_frame(
    ctx: &mut TinySkiaCpuRenderContext,
    state: &LauncherState,
    t: f64,
    titlebar_hover: TitlebarHover,
    mesh: &MeshSnapshot,
    sentinel_phase: &SentinelPhaseLocal,
    sentinel_phase_since_ms: f64,
    model: &model::ClientState,
    menu_open: bool,
) {
    // Background — mirage perforated dot grid
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, W, H);
    draw_dot_grid(ctx);

    let cx = W / 2.0;
    let cy = (H + TITLEBAR_H) / 2.0;

    // Centre cube dances for everything except On (where it parks top-left as
    // the menu button). When the centre cube is visible, the top-left shows "M".
    let centre_cube_visible = !matches!(state, LauncherState::On { .. });

    match state {
        LauncherState::On { radar_anchor_ms } => {
            let t_local = (t - radar_anchor_ms).max(0.0);
            draw_mesh_wheel(ctx, cx, cy, t, t_local, mesh);
        }
        _ => {
            draw_big_button(ctx, cx, cy, state, t, sentinel_phase, sentinel_phase_since_ms);
        }
    }

    draw_titlebar(ctx, titlebar_hover.minimize, titlebar_hover.close, !centre_cube_visible, t, sentinel_phase, sentinel_phase_since_ms);

    // Slide-down menu overlay (opens header-style under the chrome, over the canvas).
    if menu_open {
        draw_menu_overlay(ctx, model);
    }
}

/// Placeholder menu overlay — a panel that drops from under the titlebar over
/// the canvas, rendering the live ClientState model. Content is provisional
/// (owner: "throw whatever in for now, check the mechanic").
fn draw_menu_overlay(ctx: &mut TinySkiaCpuRenderContext, model: &model::ClientState) {
    // Near-opaque panel over the whole canvas area.
    ctx.set_fill_color("#0E0E11F2");
    ctx.fill_rect(0.0, TITLEBAR_H, W, H - TITLEBAR_H);
    // Left accent rail.
    ctx.set_fill_color(ACCENT);
    ctx.fill_rect(0.0, TITLEBAR_H, 2.0, H - TITLEBAR_H);

    let mut y = TITLEBAR_H + 14.0;
    let pad = 14.0;
    let mut line = |ctx: &mut TinySkiaCpuRenderContext, s: &str, color: &str, size: f64, dy: f64| {
        ctx.set_fill_color(color);
        ctx.set_font(&format!("{size}px jetbrains mono"));
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(s, pad, y);
        y += dy;
    };

    line(ctx, "MENU", ACCENT, 13.0, 22.0);

    // Connection / credential summary.
    let c = &model.conn;
    line(ctx, &format!("phase    {}", c.phase), FG0, 10.0, 15.0);
    line(ctx, &format!("monitor  {}", if c.monitor_reachable { "reachable" } else { "—" }), FG1, 10.0, 15.0);
    if let Some(cr) = &model.credential {
        let req = if cr.require { "require" } else { "open" };
        line(ctx, &format!("cred     {} g{} {}", cr.scheme, cr.generation, req), FG1, 10.0, 15.0);
    }
    line(ctx, "data     cli2", FG3, 10.0, 18.0);

    // Chains.
    line(ctx, &format!("CHAINS ({})", model.chains.len()), ACCENT, 11.0, 18.0);
    for ch in model.chains.iter().take(6) {
        let health = match ch.healthy {
            Some(true) => "ok",
            Some(false) => "down",
            None => "?",
        };
        let lat = ch.latency_ms.map(|l| format!("{l}ms")).unwrap_or_else(|| "—".into());
        let hops = ch.hops.len();
        line(ctx, &format!("· {}  {}  {}  {}h", ch.chain_id, health, lat, hops), FG1, 10.0, 14.0);
    }
}

#[derive(Default, Clone, Copy)]
struct TitlebarHover { minimize: bool, close: bool }

// ── Custom chrome titlebar (chromeless window) ───────────────────────────────

const CUBE_OUTER: &[(f64, f64)] = &[
    (20.0, 4.0), (34.0, 12.0), (34.0, 28.0),
    (20.0, 36.0), (6.0, 28.0), (6.0, 12.0), (20.0, 4.0),
];
const CUBE_INNER: &[&[(f64, f64)]] = &[
    &[(6.0, 12.0), (20.0, 20.0), (34.0, 12.0)],
    &[(20.0, 20.0), (20.0, 36.0)],
];

/// Draw a cube wireframe.
///
/// `color` — stroke color.
/// `draw_inner` — if false, only the outer hexagon is drawn.
/// `outer_alpha` — the global alpha already set by the caller; inner lines use 70% of it.
fn draw_cube(ctx: &mut TinySkiaCpuRenderContext, cx: f64, cy: f64, target: f64, color: &str, draw_inner: bool, outer_alpha: f64) {
    let scale = target / 40.0;
    let map = |p: &(f64, f64)| (cx + (p.0 - 20.0) * scale, cy + (p.1 - 20.0) * scale);

    ctx.set_stroke_color(color);
    ctx.set_stroke_width((1.5 * scale).max(1.0));
    ctx.set_line_join("round");
    ctx.begin_path();
    let first = map(&CUBE_OUTER[0]);
    ctx.move_to(first.0, first.1);
    for p in CUBE_OUTER.iter().skip(1) {
        let (x, y) = map(p);
        ctx.line_to(x, y);
    }
    ctx.stroke();

    if draw_inner {
        ctx.set_global_alpha(outer_alpha * 0.7);
        for poly in CUBE_INNER {
            ctx.begin_path();
            let first = map(&poly[0]);
            ctx.move_to(first.0, first.1);
            for p in poly.iter().skip(1) {
                let (x, y) = map(p);
                ctx.line_to(x, y);
            }
            ctx.stroke();
        }
        // Restore to outer alpha for any subsequent draw calls.
        ctx.set_global_alpha(outer_alpha);
    }
}

fn draw_titlebar(
    ctx: &mut TinySkiaCpuRenderContext,
    min_hovered: bool,
    close_hovered: bool,
    show_logo: bool,
    t: f64,
    sentinel_phase: &SentinelPhaseLocal,
    sentinel_phase_since_ms: f64,
) {
    if show_logo {
        let logo_size = 26.0;
        let cx = 14.0 + logo_size / 2.0;
        let cy = TITLEBAR_H / 2.0;

        let (cube_color, draw_inner, shimmer) = match sentinel_phase {
            SentinelPhaseLocal::Unknown
            | SentinelPhaseLocal::Starting
            | SentinelPhaseLocal::FetchingConfig => (CUBE_WHITE, false, false),
            SentinelPhaseLocal::ConfigReady => (CUBE_WHITE, true, false),
            SentinelPhaseLocal::ChildRunning => (CUBE_AMBER, true, true),
            SentinelPhaseLocal::ConfigStale => (CUBE_AMBER, true, true),
            SentinelPhaseLocal::WaitingOnboard => (CUBE_WHITE, false, false),
            SentinelPhaseLocal::Error(_) => (CUBE_RED, false, false),
        };
        let inner_alpha = match sentinel_phase {
            SentinelPhaseLocal::ConfigReady => {
                let elapsed = (t - sentinel_phase_since_ms).max(0.0);
                (elapsed / 500.0).clamp(0.0, 1.0)
            }
            _ => 1.0,
        };
        let effective_color = if matches!(
            sentinel_phase,
            SentinelPhaseLocal::ChildRunning | SentinelPhaseLocal::ConfigStale
        ) {
            let elapsed = (t - sentinel_phase_since_ms).max(0.0);
            if elapsed >= 600.0 { CUBE_AMBER } else { CUBE_WHITE }
        } else {
            cube_color
        };
        let _ = inner_alpha; // used implicitly via draw_inner fade-in on ConfigReady
        draw_cube_idle(ctx, cx, cy, logo_size, t, effective_color, draw_inner, 1.0, shimmer);
    } else {
        // Centre cube is dancing — show the "M" menu entry top-left instead of
        // the parked cube. Same hit region; opens the same overlay.
        let mx = 14.0 + 26.0 / 2.0;
        let my = TITLEBAR_H / 2.0;
        ctx.set_fill_color(ACCENT);
        ctx.set_font("16px jetbrains mono");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("M", mx, my);
    }

    let close_x = W - TITLEBAR_BTN_W;
    let min_x = W - TITLEBAR_BTN_W * 2.0;
    let cy = TITLEBAR_H / 2.0;

    if min_hovered {
        ctx.set_fill_color("#ffffff14");
        ctx.fill_rect(min_x, 0.0, TITLEBAR_BTN_W, TITLEBAR_H);
    }
    if close_hovered {
        ctx.set_fill_color("#E0565699");
        ctx.fill_rect(close_x, 0.0, TITLEBAR_BTN_W, TITLEBAR_H);
    }

    ctx.set_stroke_color(if min_hovered { FG0 } else { FG3 });
    ctx.set_stroke_width(1.2);
    ctx.begin_path();
    ctx.move_to(min_x + TITLEBAR_BTN_W / 2.0 - 6.0, cy);
    ctx.line_to(min_x + TITLEBAR_BTN_W / 2.0 + 6.0, cy);
    ctx.stroke();

    ctx.set_stroke_color(if close_hovered { FG0 } else { FG3 });
    ctx.set_stroke_width(1.2);
    let cxh = close_x + TITLEBAR_BTN_W / 2.0;
    ctx.begin_path();
    ctx.move_to(cxh - 5.0, cy - 5.0);
    ctx.line_to(cxh + 5.0, cy + 5.0);
    ctx.move_to(cxh + 5.0, cy - 5.0);
    ctx.line_to(cxh - 5.0, cy + 5.0);
    ctx.stroke();

    ctx.set_stroke_color("#ffffff10");
    ctx.set_stroke_width(1.0);
    ctx.begin_path();
    ctx.move_to(0.0, TITLEBAR_H);
    ctx.line_to(W, TITLEBAR_H);
    ctx.stroke();
}

// ── Hit region for the click target depends on state ─────────────────────────

fn click_target_rect(state: &LauncherState, sentinel_phase: &SentinelPhaseLocal) -> uzor::Rect {
    match state {
        LauncherState::On { .. } => uzor::Rect::new(0.0, TITLEBAR_H, W, H - TITLEBAR_H),
        _ => {
            // Enable click whenever sentinel has at least bootstrapped enough to
            // accept /connect.  ConfigReady = sent has chains and is ready to
            // spawn cli/tun.  ChildRunning = cli already up (Win flow).  Stale
            // = lost monitor heartbeat but local config still valid; user can
            // still toggle.  Error / WaitingOnboard handled here too.
            let enabled = matches!(
                sentinel_phase,
                SentinelPhaseLocal::ConfigReady
                    | SentinelPhaseLocal::ChildRunning
                    | SentinelPhaseLocal::ConfigStale
                    | SentinelPhaseLocal::Error(_)
                    | SentinelPhaseLocal::WaitingOnboard
            );
            if !enabled {
                // Return a zero-area rect so clicks are never detected.
                return uzor::Rect::new(0.0, 0.0, 0.0, 0.0);
            }
            let cx = W / 2.0;
            let cy = (H + TITLEBAR_H) / 2.0;
            let r = CUBE_OFF_SIZE;
            uzor::Rect::new(cx - r, cy - r, r * 2.0, r * 2.0)
        }
    }
}

// ── Status polling for connect debounce ──────────────────────────────────────

#[derive(serde::Deserialize)]
struct SentinelStatusPoll {
    #[serde(default)]
    tun: Option<TunStatusPoll>,
}

#[derive(serde::Deserialize)]
struct TunStatusPoll {
    alive: bool,
}

/// Poll `GET /status` until `tun.alive == true` or timeout.
/// Returns `true` if tun came up, `false` on timeout.
async fn wait_for_tun_alive(
    client: &reqwest::Client,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        let alive = client
            .get(format!("{SENTINEL_BASE}/status"))
            .send()
            .await
            .ok()
            .and_then(|r| {
                if r.status().is_success() {
                    Some(r)
                } else {
                    None
                }
            });

        if let Some(resp) = alive {
            if let Ok(status) = resp.json::<SentinelStatusPoll>().await {
                if status.tun.map(|t| t.alive).unwrap_or(false) {
                    return true;
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

// ── winit ApplicationHandler ─────────────────────────────────────────────────

impl ApplicationHandler for LauncherHandler {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.app.is_some() {
            return;
        }
        tracing::info!("[stage] resumed: creating window");

        let icon = load_icon();

        let mut attrs = Window::default_attributes()
            .with_title("Mirage")
            .with_inner_size(LogicalSize::new(W as u32, H as u32))
            .with_resizable(false)
            .with_decorations(false);
        if let Some(ic) = icon {
            attrs = attrs.with_window_icon(Some(ic));
        }

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!("create_window failed: {e}");
                event_loop.exit();
                return;
            }
        };
        tracing::info!("[stage] window created");

        let context = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("softbuffer Context::new failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let surface = match softbuffer::Surface::new(&context, window.clone()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("softbuffer Surface::new failed: {e}");
                event_loop.exit();
                return;
            }
        };
        tracing::info!("[stage] softbuffer ready");

        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let now = Instant::now();

        let dns_resolver = Arc::new(DnsResolver::new(self.rt.handle()));

        let cfg_path = exe_dir().join("client-config.json");
        let mesh_rx = mesh_state::spawn_poller(self.rt.handle(), cfg_path, dns_resolver.clone());
        let sentinel_rx = sentinel_status::spawn_poller(self.rt.handle());
        let model_rx = model::spawn_model_poller(self.rt.handle(), exe_dir());

        ensure_sentinel_toml();
        ensure_client_template_cli2();
        ensure_orchestrator_toml();

        // Defer orchestrator spawn by 3s so the window paints first — testers see
        // the GUI immediately, and any crash that follows is logged but
        // does NOT block window creation.
        let initial_sentinel: Option<std::process::Child> = None;
        if model::mock_enabled() {
            tracing::info!("[mock] sandbox — NOT spawning the real backend (orchestrator/sentinel/cli2/tun)");
        } else {
            tracing::info!("[stage] window up — deferring orchestrator spawn 3s");
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                tracing::info!("[stage] deferred: spawning orchestrator");
                match spawn_orchestrator() {
                    Ok(_c) => tracing::info!("[stage] orchestrator spawned"),
                    Err(e) => tracing::warn!("failed to spawn orchestrator: {}", e),
                }
            });
        }

        // If a mirage:// URL was passed on the CLI, POST it to sentinel
        // once sentinel's HTTP server becomes reachable (poll up to 15s).
        if let Some(url) = self.cli_onboard_url.take() {
            self.rt.spawn(async move {
                let client = match reqwest::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("cli onboard: failed to build client: {e}");
                        return;
                    }
                };
                // Wait for sentinel HTTP to be up (max 15s).
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                loop {
                    if tokio::time::Instant::now() >= deadline {
                        tracing::warn!("cli onboard: sentinel did not become reachable in 15s");
                        return;
                    }
                    if client.get(format!("{SENTINEL_BASE}/health")).send().await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                post_onboard_url(url).await;
            });
        }

        let sentinel_arc = Arc::new(Mutex::new(initial_sentinel));

        // Watchdog task: checks every 1500ms if sentinel exited; respawns up
        // to 5 times in 60s before giving up. Skipped in the mock sandbox.
        if !model::mock_enabled() {
            spawn_orchestrator_watchdog(self.rt.handle(), Arc::downgrade(&sentinel_arc));
        }

        self.app = Some(LauncherApp {
            window,
            surface,
            mouse: MouseAccum::default(),
            last_frame: now,
            startup: now,
            state: LauncherState::Off,
            sentinel: sentinel_arc,
            event_rx,
            event_tx,
            rt: self.rt.clone(),
            coordinator: uzor::InputCoordinator::new(),
            mesh_rx,
            sentinel_rx,
            _dns_resolver: dns_resolver,
            sentinel_phase_since_ms: 0.0,
            sentinel_phase_prev: SentinelPhaseLocal::Unknown,
            _model_rx: model_rx,
            menu_open: false,
        });
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(app) = self.app.as_mut() else { return };

        match event {
            WindowEvent::CloseRequested => {
                // Kill sentinel; then reap any cli/tun the sentinel spawned —
                // JobObject is disabled (it could kill launcher itself), so
                // children are not auto-killed when sentinel dies.
                let owned_sentinel = app.sentinel.lock().expect("not poisoned").take();
                if let Some(mut s) = owned_sentinel {
                    let _ = s.kill();
                    let _ = s.wait();
                }
                reap_orphans();
                event_loop.exit();
            }

            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let dt = now.duration_since(app.last_frame).as_secs_f64();
                app.last_frame = now;
                let t = now.duration_since(app.startup).as_secs_f64() * 1000.0;

                // Drain async events
                while let Ok(ev) = app.event_rx.try_recv() {
                    match ev {
                        LauncherEvent::Started => {
                            let anchor = match app.state {
                                LauncherState::Connecting { started_at_ms } => started_at_ms,
                                _ => t,
                            };
                            app.state = LauncherState::On { radar_anchor_ms: anchor };
                        }
                        LauncherEvent::Failed(msg) => {
                            app.state = LauncherState::Error(msg);
                        }
                        LauncherEvent::Stopped => {
                            app.state = LauncherState::Off;
                        }
                    }
                }

                // Update sentinel phase and track transition time.
                let current_sentinel = app.sentinel_rx.borrow().clone();
                if current_sentinel != app.sentinel_phase_prev {
                    app.sentinel_phase_since_ms = t;
                    app.sentinel_phase_prev = current_sentinel.clone();

                    // Serialize the model snapshot to JSON on every sentinel-phase
                    // transition.  This gives testers a structured state dump in
                    // the launcher log and ensures all model fields are live
                    // (serde Serialize reads every field through its generated impl).
                    let model_snap = app._model_rx.borrow().clone();
                    if tracing::enabled!(tracing::Level::DEBUG) {
                        if let Ok(json) = serde_json::to_string(&*model_snap) {
                            tracing::debug!("[model] phase transition: {json}");
                        }
                    }
                }

                let size = app.window.inner_size();
                let width = size.width;
                let height = size.height;
                if width == 0 || height == 0 {
                    app.window.request_redraw();
                    return;
                }

                app.surface
                    .resize(
                        NonZeroU32::new(width).expect("> 0"),
                        NonZeroU32::new(height).expect("> 0"),
                    )
                    .expect("resize always succeeds on valid dimensions");

                let mut input = InputState::new();
                input.pointer.pos = app.mouse.pos;
                input.pointer.button_down = app.mouse.button_down;
                input.pointer.clicked = app.mouse.clicked.take();
                input.dt = dt;
                input.time = t / 1000.0;

                app.coordinator.begin_frame(input);

                let close_x = W - TITLEBAR_BTN_W;
                let min_x = W - TITLEBAR_BTN_W * 2.0;
                let drag_w = (min_x - 0.0).max(0.0);
                app.coordinator.register(
                    "tb_drag",
                    uzor::Rect::new(0.0, 0.0, drag_w, TITLEBAR_H),
                    uzor::input::Sense::CLICK,
                );
                app.coordinator.register(
                    "tb_min",
                    uzor::Rect::new(min_x, 0.0, TITLEBAR_BTN_W, TITLEBAR_H),
                    uzor::input::Sense::CLICK,
                );
                app.coordinator.register(
                    "tb_close",
                    uzor::Rect::new(close_x, 0.0, TITLEBAR_BTN_W, TITLEBAR_H),
                    uzor::input::Sense::CLICK,
                );
                app.coordinator.register(
                    "btn",
                    click_target_rect(&app.state, &current_sentinel),
                    uzor::input::Sense::CLICK,
                );

                let responses = app.coordinator.end_frame();

                let mut tb_hover = TitlebarHover::default();
                let mut tb_min_click = false;
                let mut tb_close_click = false;
                for (id, resp) in &responses {
                    match id.as_str() {
                        "tb_min" => {
                            tb_hover.minimize = resp.hovered;
                            if resp.clicked { tb_min_click = true; }
                        }
                        "tb_close" => {
                            tb_hover.close = resp.hovered;
                            if resp.clicked { tb_close_click = true; }
                        }
                        _ => {}
                    }
                }
                if tb_close_click {
                    let owned_sentinel = app.sentinel.lock().expect("not poisoned").take();
                    if let Some(mut s) = owned_sentinel {
                        let _ = s.kill();
                        let _ = s.wait();
                    }
                    event_loop.exit();
                    return;
                }
                if tb_min_click {
                    app.window.set_minimized(true);
                }

                let dpr = app.window.scale_factor();
                let mut ctx = TinySkiaCpuRenderContext::new(width, height, dpr);
                let mesh_snapshot = app.mesh_rx.borrow().clone();
                let model_snapshot = app._model_rx.borrow().clone();
                draw_frame(
                    &mut ctx,
                    &app.state,
                    t,
                    tb_hover,
                    &mesh_snapshot,
                    &current_sentinel,
                    app.sentinel_phase_since_ms,
                    &model_snapshot,
                    app.menu_open,
                );
                for (id, resp) in &responses {
                    if !resp.clicked || id.as_str() != "btn" {
                        continue;
                    }

                    // WaitingOnboard: paste onboarding link from clipboard.
                    if matches!(&current_sentinel, SentinelPhaseLocal::WaitingOnboard) {
                        if let Some(url) = prompt_for_onboard_link() {
                            app.rt.spawn(post_onboard_url(url));
                        }
                        continue;
                    }

                    match &app.state {
                        LauncherState::Off | LauncherState::Error(_) => {
                            app.state = LauncherState::Connecting { started_at_ms: t };
                            let tx = app.event_tx.clone();
                            if model::mock_enabled() {
                                // Mock sandbox: simulate the connect handshake (no backend).
                                let txm = app.event_tx.clone();
                                app.rt.spawn(async move {
                                    tokio::time::sleep(Duration::from_millis(1600)).await;
                                    let _ = txm.send(LauncherEvent::Started);
                                });
                                continue;
                            }
                            app.rt.spawn(async move {
                                let client = reqwest::Client::builder()
                                    .timeout(Duration::from_secs(5))
                                    .build()
                                    .expect("reqwest client always builds");

                                match client
                                    .post(format!("{SENTINEL_BASE}/connect"))
                                    .send()
                                    .await
                                {
                                    Ok(r) if r.status().is_success() || r.status().as_u16() == 409 => {
                                        // 409 = tun already running — that's fine, debounce below.
                                    }
                                    Ok(r) => {
                                        let _ = tx.send(LauncherEvent::Failed(format!(
                                            "connect: {}",
                                            r.status()
                                        )));
                                        return;
                                    }
                                    Err(e) => {
                                        let _ = tx.send(LauncherEvent::Failed(format!(
                                            "sentinel unreachable: {}",
                                            e
                                        )));
                                        return;
                                    }
                                }

                                // Debounce — wait up to 10s for tun.alive.
                                if wait_for_tun_alive(&client, Duration::from_secs(10)).await {
                                    let _ = tx.send(LauncherEvent::Started);
                                } else {
                                    let _ = tx.send(LauncherEvent::Failed(
                                        "tun didn't come up (timeout 10s)".into(),
                                    ));
                                }
                            });
                        }
                        LauncherState::On { .. } => {
                            let tx = app.event_tx.clone();
                            if model::mock_enabled() {
                                // Mock sandbox: disconnect instantly (no backend).
                                let _ = app.event_tx.send(LauncherEvent::Stopped);
                                continue;
                            }
                            app.rt.spawn(async move {
                                let client = reqwest::Client::builder()
                                    .timeout(Duration::from_secs(5))
                                    .build()
                                    .expect("reqwest client always builds");
                                let _ = client
                                    .post(format!("{SENTINEL_BASE}/disconnect"))
                                    .send()
                                    .await;
                                let _ = tx.send(LauncherEvent::Stopped);
                            });
                        }
                        LauncherState::Connecting { .. } => {}
                    }
                }

                let pixels = ctx.pixels();
                let mut sb_buf = app.surface.buffer_mut().expect("buffer_mut always succeeds");
                let pixel_count = (width * height) as usize;
                for i in 0..pixel_count {
                    let base = i * 4;
                    let r = pixels[base] as u32;
                    let g = pixels[base + 1] as u32;
                    let b = pixels[base + 2] as u32;
                    sb_buf[i] = (r << 16) | (g << 8) | b;
                }
                sb_buf.present().expect("present always succeeds");
                app.window.request_redraw();
            }

            WindowEvent::CursorMoved { position, .. } => {
                let scale = app.window.scale_factor();
                app.mouse.pos = Some((position.x / scale, position.y / scale));
            }

            WindowEvent::CursorLeft { .. } => {
                app.mouse.pos = None;
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let uzor_btn = match button {
                    MouseButton::Left => UzorMouseButton::Left,
                    MouseButton::Right => UzorMouseButton::Right,
                    MouseButton::Middle => UzorMouseButton::Middle,
                    _ => return,
                };

                if button == MouseButton::Left && state == ElementState::Pressed {
                    if let Some((x, y)) = app.mouse.pos {
                        // Top-left "M" / parked-cube → toggle the menu overlay.
                        if y < TITLEBAR_H && x < 44.0 {
                            app.menu_open = !app.menu_open;
                            return;
                        }
                        if y < TITLEBAR_H && x < W - TITLEBAR_BTN_W * 2.0 {
                            let _ = app.window.drag_window();
                            return;
                        }
                    }
                }

                match state {
                    ElementState::Pressed => {
                        app.mouse.button_down = Some(uzor_btn);
                    }
                    ElementState::Released => {
                        app.mouse.button_down = None;
                        app.mouse.clicked = Some(uzor_btn);
                    }
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(app) = self.app.as_ref() {
            app.window.request_redraw();
        }
    }
}

/// Decode the bundled PNG into a `winit::window::Icon` (RGBA8).
fn load_icon() -> Option<Icon> {
    let img = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Icon::from_rgba(rgba.into_raw(), w, h).ok()
}

// ── Sentinel watchdog ────────────────────────────────────────────────────────

/// Burst limit: at most this many respawns within `RESPAWN_BURST_WINDOW`.
const RESPAWN_BURST_LIMIT: u32 = 5;
/// Window duration for burst counting.
const RESPAWN_BURST_WINDOW: Duration = Duration::from_secs(60);
/// Cooldown between respawn attempts (avoids tight loops on persistent failures).
const RESPAWN_COOLDOWN: Duration = Duration::from_millis(500);
/// How often the watchdog checks the sentinel child status.
const WATCHDOG_TICK: Duration = Duration::from_millis(1500);

/// Spawns the background watchdog task.  Holds a `Weak` reference to the
/// shared orchestrator handle so the task exits naturally when the app drops.
fn spawn_orchestrator_watchdog(
    rt: &tokio::runtime::Handle,
    sentinel: std::sync::Weak<Mutex<Option<std::process::Child>>>,
) {
    rt.spawn(async move {
        let mut burst_count: u32 = 0;
        let mut burst_window_start = tokio::time::Instant::now();

        loop {
            tokio::time::sleep(WATCHDOG_TICK).await;

            let arc = match sentinel.upgrade() {
                Some(a) => a,
                None => return, // app dropped — exit cleanly
            };

            // Check if sentinel has exited.
            let exited = {
                let mut guard = arc.lock().expect("not poisoned");
                match guard.as_mut() {
                    None => false, // no child (already taken for kill)
                    Some(child) => {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                tracing::warn!(
                                    "orchestrator exited (code {:?}) — preparing respawn",
                                    status.code()
                                );
                                true
                            }
                            Ok(None) => false, // still running
                            Err(e) => {
                                tracing::warn!("sentinel try_wait error: {e}");
                                false
                            }
                        }
                    }
                }
            };

            if !exited {
                continue;
            }

            // Reset burst window if it has expired.
            if burst_window_start.elapsed() >= RESPAWN_BURST_WINDOW {
                burst_count = 0;
                burst_window_start = tokio::time::Instant::now();
            }

            if burst_count >= RESPAWN_BURST_LIMIT {
                tracing::warn!(
                    "orchestrator respawned {} times in {:?} — giving up; close and reopen launcher",
                    burst_count,
                    RESPAWN_BURST_WINDOW,
                );
                // Drop the dead child handle but do not respawn.
                arc.lock().expect("not poisoned").take();
                return;
            }

            tokio::time::sleep(RESPAWN_COOLDOWN).await;

            // Upgrade reference again after sleeping (app might have closed).
            let arc2 = match sentinel.upgrade() {
                Some(a) => a,
                None => return,
            };

            match spawn_orchestrator() {
                Ok(child) => {
                    burst_count += 1;
                    tracing::warn!(
                        "orchestrator respawned (burst {}/{})",
                        burst_count,
                        RESPAWN_BURST_LIMIT
                    );
                    *arc2.lock().expect("not poisoned") = Some(child);
                }
                Err(e) => {
                    tracing::warn!("failed to respawn orchestrator: {e}");
                    burst_count += 1;
                }
            }
        }
    });
}

// ── Onboarding (WaitingOnboard) ──────────────────────────────────────────────

/// Read the clipboard and return the text if it looks like an onboarding URL.
/// On non-Windows always returns `None`.
fn prompt_for_onboard_link() -> Option<String> {
    #[cfg(windows)]
    {
        use clipboard_win::{formats, get_clipboard};
        let text: String = get_clipboard(formats::Unicode).ok()?;
        if text.starts_with("mirage://onboard?") {
            Some(text.trim().to_owned())
        } else {
            None
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// POST the onboarding URL to sentinel.  Fire-and-forget (caller does not await).
async fn post_onboard_url(url: String) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("onboard: failed to build client: {e}");
            return;
        }
    };

    #[derive(serde::Serialize)]
    struct OnboardPayload<'a> {
        url: &'a str,
    }

    match client
        .post(format!("{SENTINEL_BASE}/onboard"))
        .json(&OnboardPayload { url: &url })
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            tracing::info!("onboard: accepted by sentinel");
        }
        Ok(r) => {
            tracing::warn!("onboard: sentinel returned {}", r.status());
        }
        Err(e) => {
            tracing::warn!("onboard: request failed: {e}");
        }
    }
}

// ── Entry ────────────────────────────────────────────────────────────────────

fn main() {
    // Tracing → file in exe dir + panic hook so crashes leave a trace.
    // Without this, windows_subsystem="windows" eats stderr and the app
    // disappears silently — testers cannot diagnose.
    let log_path = exe_dir().join("mirage-launcher.log");
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(f))
            .with_ansi(false)
            .try_init();
    } else {
        tracing_subscriber::fmt::init();
    }

    let log_path_for_panic = log_path.clone();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        let msg = format!("{info}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true).open(&log_path_for_panic)
        {
            let _ = writeln!(f, "[PANIC] {}", msg);
        }
        tracing::error!("launcher panic: {msg}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }));

    tracing::info!("launcher start: log={:?}", log_path);

    #[cfg(windows)]
    jobobj::init();

    // Detect a mirage://... URL passed as a CLI argument (registered URL
    // scheme handler — Windows opens us with the full URL as argv[1]).
    let cli_onboard_url: Option<String> = std::env::args()
        .skip(1)
        .find(|a| a.starts_with("mirage://"))
        .map(|s| s.trim().to_owned());

    // `--mock` CLI flag → offline sandbox. A CLI arg survives UAC elevation
    // (manifest requireAdministrator), whereas an env var may not, so the flag
    // is the reliable way to engage the mock on the elevated launcher. We set
    // the env var that `model::mock_enabled()` reads so both paths converge.
    if std::env::args().skip(1).any(|a| a == "--mock") {
        std::env::set_var("MIRAGE_LAUNCHER_MOCK", "1");
        tracing::info!("[mock] --mock flag → offline sandbox engaged");
    }

    tracing::info!("[stage] tokio runtime build");
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            tracing::error!("tokio runtime build failed: {e}");
            std::thread::sleep(std::time::Duration::from_millis(500));
            std::process::exit(1);
        }
    };
    tracing::info!("[stage] tokio runtime ready");

    tracing::info!("[stage] winit event loop init");
    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            tracing::error!("winit EventLoop::new failed: {e}");
            std::thread::sleep(std::time::Duration::from_millis(500));
            std::process::exit(1);
        }
    };
    event_loop.set_control_flow(ControlFlow::Poll);
    tracing::info!("[stage] winit event loop ready, entering run_app");

    let mut handler = LauncherHandler { app: None, rt, cli_onboard_url };
    if let Err(e) = event_loop.run_app(&mut handler) {
        tracing::error!("event loop run_app returned error: {e}");
    }
    tracing::info!("[stage] event loop exited");
}
