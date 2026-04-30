#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::f64::consts::{PI, TAU};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(windows)]
mod jobobj {
    //! Wrap launcher + spawned children in a Windows Job Object with
    //! KILL_ON_JOB_CLOSE — any exit of the launcher cascades a forced kill
    //! to every child, even if our shutdown code never runs.

    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    struct JobHandle(HANDLE);
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    static JOB: OnceLock<JobHandle> = OnceLock::new();

    pub fn init() {
        JOB.get_or_init(|| unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return JobHandle(std::ptr::null_mut());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation = JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..std::mem::zeroed()
            };
            let _ = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            let _ = AssignProcessToJobObject(h, GetCurrentProcess());
            JobHandle(h)
        });
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
const DOT_STEP: f64 = 27.0;
const SURFACE: &str = "#14151A";
const BORDER: &str = "#ffffff12";  // rgba(255,255,255,0.07)
const BORDERS: &str = "#ffffff24"; // rgba(255,255,255,0.14)
const FG0: &str = "#F4F4F5";
const FG3: &str = "#555860";
const ACCENT: &str = "#FBB26A";
const ACCENT_HALO: &str = "#FBB26A4D"; // rgba(251,178,106,0.3)
const L1: &str = "#6DB8F5";
const WARN: &str = "#E8A838";

const W: f64 = 380.0;
const H: f64 = 540.0;

/// Shared "outer ring" radius — used by both the OFF/Connecting button and
/// the mesh-wheel scaffold so the visual size never jumps when state changes.
const MESH_OUTER: f64 = 130.0;

/// Custom-chrome titlebar height (logical pixels).  Top of the window is a
/// drag region except for the close/minimise buttons on the right.
const TITLEBAR_H: f64 = 36.0;
const TITLEBAR_BTN_W: f64 = 44.0;

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

// ── Child process pair (cli + tun) ───────────────────────────────────────────

struct ChildPair {
    cli: tokio::process::Child,
    tun: tokio::process::Child,
}

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

async fn spawn_pair() -> anyhow::Result<ChildPair> {
    use std::os::windows::process::CommandExt;

    let dir = exe_dir();
    let cli_exe = dir.join("mirage-client-cli.exe");
    let tun_exe = dir.join("mirage2tun.exe");
    let config = dir.join("client-config.json");

    let cli = tokio::process::Command::new(&cli_exe)
        .arg("--config").arg(&config)
        .current_dir(&dir)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;

    tokio::time::sleep(Duration::from_secs(2)).await;

    let tun = tokio::process::Command::new(&tun_exe)
        .arg("debug-run")
        .current_dir(&dir)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;

    Ok(ChildPair { cli, tun })
}

async fn kill_pair(p: &mut ChildPair) {
    let _ = p.tun.kill().await;
    let _ = p.cli.kill().await;
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
    children: Arc<Mutex<Option<ChildPair>>>,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<LauncherEvent>,
    event_tx: tokio::sync::mpsc::UnboundedSender<LauncherEvent>,
    rt: Arc<tokio::runtime::Runtime>,
    coordinator: uzor::InputCoordinator,
}

struct LauncherHandler {
    app: Option<LauncherApp>,
    rt: Arc<tokio::runtime::Runtime>,
}

// ── Mesh wheel — pixel-perfect port of ClientApp.jsx MeshWheel ───────────────
//
// Geometry (from JSX):
//   svg viewBox="-170 -170 340 340", width=320, height=320
//   R (outer ring) = 130
//   rL1 (middle ring) = 86
//   L2: 6 nodes at angles (i/6)*2π - π/2  (top first, going CW)
//   L1: 3 nodes at angles (i/3)*2π + π/2  (bottom first)
//   hot_l2 = floor(t/750) % 6
//   hot_l1 = floor(t/1100) % 3
//
// Drawn into our local `cx,cy` center.  `t` is ms since startup.

fn draw_mesh_wheel(ctx: &mut TinySkiaCpuRenderContext, cx: f64, cy: f64, t: f64, t_local: f64) {
    let r_outer = MESH_OUTER;
    let r_l1 = 86.0_f64;

    // L2 positions (6 nodes, top first, CW)
    let l2: Vec<(f64, f64, usize)> = (0..6)
        .map(|i| {
            let a = (i as f64 / 6.0) * TAU - PI / 2.0;
            (cx + a.cos() * r_outer, cy + a.sin() * r_outer, i)
        })
        .collect();

    // L1 positions (3 nodes, bottom first, CW)
    let l1: Vec<(f64, f64, usize)> = (0..3)
        .map(|i| {
            let a = (i as f64 / 3.0) * TAU + PI / 2.0;
            (cx + a.cos() * r_l1, cy + a.sin() * r_l1, i)
        })
        .collect();

    let hot_l2 = ((t / 750.0).floor() as i64).rem_euclid(6) as usize;
    let hot_l1 = ((t / 1100.0).floor() as i64).rem_euclid(3) as usize;

    // ── radar echo from center (1-2-3-pause rhythm) ─────────────────────────
    // Doppler radar covers the entire window (passes through the titlebar too).
    draw_doppler(ctx, cx, cy, FULL_RADAR_R, t_local, ACCENT);

    // ── scaffold rings (dashed) ─────────────────────────────────────────────
    ctx.set_stroke_color(BORDER);
    ctx.set_stroke_width(1.0);
    ctx.set_line_dash(&[1.0, 4.0]);
    ctx.begin_path();
    ctx.arc(cx, cy, r_outer, 0.0, TAU);
    ctx.stroke();
    ctx.begin_path();
    ctx.arc(cx, cy, r_l1, 0.0, TAU);
    ctx.stroke();
    ctx.set_line_dash(&[]);

    // ── 48 tick marks around outer ring (every 12th is taller) ──────────────
    ctx.set_stroke_color(FG3);
    ctx.set_stroke_width(0.6);
    ctx.set_global_alpha(0.5);
    for i in 0..48u32 {
        let a = (i as f64 / 48.0) * TAU;
        let r1 = r_outer + 3.0;
        let r2 = r_outer + if i % 12 == 0 { 10.0 } else { 6.0 };
        ctx.begin_path();
        ctx.move_to(cx + a.cos() * r1, cy + a.sin() * r1);
        ctx.line_to(cx + a.cos() * r2, cy + a.sin() * r2);
        ctx.stroke();
    }
    ctx.set_global_alpha(1.0);

    // ── center → L1 lines (one per L1 node; hot one is solid blue) ──────────
    for &(lx, ly, i) in &l1 {
        let is_hot = i == hot_l1;
        let (color, width, dash) = if is_hot {
            (L1, 1.4_f64, vec![])
        } else {
            ("#6DB8F540", 0.8_f64, vec![2.0_f64, 3.0])
        };
        ctx.set_stroke_color(color);
        ctx.set_stroke_width(width);
        ctx.set_line_dash(&dash);
        ctx.begin_path();
        ctx.move_to(cx, cy);
        ctx.line_to(lx, ly);
        ctx.stroke();
    }
    ctx.set_line_dash(&[]);

    // ── L1 → L2 lines (each L2 connects to its NEAREST L1 — JSX logic) ──────
    for &(x2, y2, i) in &l2 {
        // find nearest L1
        let mut best: (f64, f64) = (l1[0].0, l1[0].1);
        let mut best_d = (best.0 - x2).hypot(best.1 - y2);
        for &(lx, ly, _) in &l1 {
            let d = (lx - x2).hypot(ly - y2);
            if d < best_d {
                best = (lx, ly);
                best_d = d;
            }
        }

        let is_hot = i == hot_l2;
        let (color, width, dash) = if is_hot {
            (ACCENT, 1.4_f64, vec![])
        } else {
            ("#FBB26A38", 0.8_f64, vec![2.0_f64, 3.0])
        };
        ctx.set_stroke_color(color);
        ctx.set_stroke_width(width);
        ctx.set_line_dash(&dash);
        ctx.begin_path();
        ctx.move_to(best.0, best.1);
        ctx.line_to(x2, y2);
        ctx.stroke();
    }
    ctx.set_line_dash(&[]);

    // ── L2 nodes (circles) ──────────────────────────────────────────────────
    for &(nx, ny, i) in &l2 {
        let is_hot = i == hot_l2;
        let border = if is_hot { ACCENT } else { BORDERS };
        let dot = if is_hot { ACCENT } else { FG3 };
        let bw = if is_hot { 1.6 } else { 1.0 };

        // outer halo when hot
        if is_hot {
            ctx.set_global_alpha(0.5);
            ctx.set_stroke_color(ACCENT);
            ctx.set_stroke_width(1.0);
            ctx.begin_path();
            ctx.arc(nx, ny, 14.0, 0.0, TAU);
            ctx.stroke();
            ctx.set_global_alpha(1.0);
        }

        // body
        ctx.set_fill_color(SURFACE);
        ctx.begin_path();
        ctx.arc(nx, ny, 10.0, 0.0, TAU);
        ctx.fill();

        ctx.set_stroke_color(border);
        ctx.set_stroke_width(bw);
        ctx.begin_path();
        ctx.arc(nx, ny, 10.0, 0.0, TAU);
        ctx.stroke();

        // center dot
        ctx.set_fill_color(dot);
        ctx.begin_path();
        ctx.arc(nx, ny, 2.5, 0.0, TAU);
        ctx.fill();
    }

    // ── L1 nodes (squares 14x14) ────────────────────────────────────────────
    for &(nx, ny, i) in &l1 {
        let is_hot = i == hot_l1;
        let border = if is_hot { L1 } else { BORDERS };
        let dot = if is_hot { L1 } else { FG3 };
        let bw = if is_hot { 1.6 } else { 1.0 };

        ctx.set_fill_color(SURFACE);
        ctx.fill_rect(nx - 7.0, ny - 7.0, 14.0, 14.0);

        ctx.set_stroke_color(border);
        ctx.set_stroke_width(bw);
        ctx.stroke_rect(nx - 7.0, ny - 7.0, 14.0, 14.0);

        ctx.set_fill_color(dot);
        ctx.begin_path();
        ctx.arc(nx, ny, 2.0, 0.0, TAU);
        ctx.fill();
    }

    // ── center user dot ─────────────────────────────────────────────────────
    ctx.set_fill_color(ACCENT);
    ctx.begin_path();
    ctx.arc(cx, cy, 6.0, 0.0, TAU);
    ctx.fill();
}

// ── Big OFF button (180px circle) ────────────────────────────────────────────

/// Doppler-style outgoing pulses — 3 rings cycling continuously.
///
/// `t_local` is time since the radar was armed (ms).  We always start with
/// ring 1 emerging from the centre at t=0 — no off-screen rings inherited
/// from a free-running clock.  Subsequent rings stagger so a new one starts
/// when the previous reaches roughly a third of the way out.
fn draw_doppler(
    ctx: &mut TinySkiaCpuRenderContext,
    cx: f64,
    cy: f64,
    r_max: f64,
    t_local: f64,
    color: &str,
) {
    // Ring expansion lasts 4 s (slow flight), rings spawn every 1300 ms so
    // up to three are airborne simultaneously and a fresh wave is visible
    // every ~1.3 s — never a 6-second pause between sightings.
    const CYCLE: f64 = 4000.0;
    const SPAWN: f64 = 1300.0;
    const OFFSETS: [f64; 3] = [0.0, SPAWN, SPAWN * 2.0];

    for offset in OFFSETS {
        let since = t_local - offset;
        if since < 0.0 { continue; }      // ring hasn't been emitted yet
        let local = since.rem_euclid(CYCLE);
        let phase = local / CYCLE; // 0..1
        // Ease-out cubic — fast start, slow end (matches CSS ease-out).
        let eased = 1.0 - (1.0 - phase).powi(3);
        let r = r_max * eased;
        let alpha = 1.0 - phase;

        // Soft pulse — three concentric strokes of decreasing alpha & width
        // simulate a soft glow without per-pixel blur.
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
/// `0` means the cube vanishes on the same frame as the click.
const CUBE_DISSOLVE_MS: f64 = 0.0;

/// Render the cube with a heat-haze + double-exposure phantom drift.
///
/// Approximation of the design system's `mirage · heat-haze` + `double exposure`
/// motion: two faded phantom copies drift left/right out of phase, the main
/// cube has a tiny per-frame jitter to mimic feTurbulence/feDisplacementMap.
/// `alpha` scales every layer (used to dissolve the cube on click).
fn draw_cube_idle(
    ctx: &mut TinySkiaCpuRenderContext,
    cx: f64,
    cy: f64,
    size: f64,
    t: f64,
    alpha: f64,
) {
    // drift period 2s, ±3 logical px — matches the JSX `@keyframes drift`.
    let drift = (t * std::f64::consts::PI / 1000.0).sin() * 3.0;

    // Phantom A — amber, drifts +X, slightly faded.
    ctx.set_global_alpha(alpha * 0.35);
    draw_cube(ctx, cx + drift, cy, size);

    // Phantom B — amber, drifts -X with phase offset, even fainter.
    let drift2 = ((t + 500.0) * std::f64::consts::PI / 1000.0).sin() * 3.0;
    ctx.set_global_alpha(alpha * 0.25);
    draw_cube(ctx, cx - drift2, cy, size);

    // Main cube — small shimmer jitter (feTurbulence approximation).
    let jx = ((t * 0.013).sin() + (t * 0.019).cos()) * 0.6;
    let jy = ((t * 0.011).cos() + (t * 0.017).sin()) * 0.6;
    ctx.set_global_alpha(alpha);
    draw_cube(ctx, cx + jx, cy + jy, size);

    ctx.set_global_alpha(1.0);
}

fn draw_big_button(
    ctx: &mut TinySkiaCpuRenderContext,
    cx: f64,
    cy: f64,
    state: &LauncherState,
    t: f64,
) {
    match state {
        LauncherState::Off => {
            draw_cube_idle(ctx, cx, cy, CUBE_OFF_SIZE, t, 1.0);
        }
        LauncherState::Error(_) => {
            // Same cube, slightly desaturated tint via reduced alpha.
            draw_cube_idle(ctx, cx, cy, CUBE_OFF_SIZE, t, 0.7);
        }
        LauncherState::Connecting { started_at_ms } => {
            let elapsed = (t - started_at_ms).max(0.0);

            // Cube vanishes the instant the user clicks — no slow fade.
            // We only redraw it during a single CUBE_DISSOLVE_MS frame so the
            // shimmer/drift doesn't visually freeze on top of the radar.
            if elapsed < CUBE_DISSOLVE_MS {
                let dissolve = elapsed / CUBE_DISSOLVE_MS;
                draw_cube_idle(ctx, cx, cy, CUBE_OFF_SIZE, t, 1.0 - dissolve);
            }

            // Doppler radar — phase anchored to the click moment so the very
            // first ring always emerges from the centre, never mid-flight.
            draw_doppler(ctx, cx, cy, FULL_RADAR_R, elapsed, ACCENT);
        }
        LauncherState::On { .. } => unreachable!(),
    }
}

// ── Center STOP button (110px circle, drawn inside mesh) ─────────────────────

fn draw_center_stop_button(ctx: &mut TinySkiaCpuRenderContext, cx: f64, cy: f64) {
    let r = 55.0;

    // Translucent fill
    ctx.set_fill_color("#FBB26A14"); // rgba(251,178,106,0.08)
    ctx.begin_path();
    ctx.arc(cx, cy, r, 0.0, TAU);
    ctx.fill();

    ctx.set_stroke_color(ACCENT);
    ctx.set_stroke_width(1.5);
    ctx.begin_path();
    ctx.arc(cx, cy, r, 0.0, TAU);
    ctx.stroke();

    ctx.set_fill_color(ACCENT);
    ctx.set_font("bold 10px jetbrains mono");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("STOP", cx, cy);
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
) {
    // Background — mirage perforated dot grid
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, W, H);
    draw_dot_grid(ctx);

    let cx = W / 2.0;
    // Visual centre is shifted down by half the titlebar so the button isn't
    // visually pushed up by the chrome strip.
    let cy = (H + TITLEBAR_H) / 2.0;

    match state {
        LauncherState::On { radar_anchor_ms } => {
            let t_local = (t - radar_anchor_ms).max(0.0);
            draw_mesh_wheel(ctx, cx, cy, t, t_local);
        }
        _ => {
            draw_big_button(ctx, cx, cy, state, t);
        }
    }

    // Custom titlebar last so it overlays everything.
    draw_titlebar(ctx, titlebar_hover.minimize, titlebar_hover.close);
}

#[derive(Default, Clone, Copy)]
struct TitlebarHover { minimize: bool, close: bool }

// ── Custom chrome titlebar (chromeless window) ───────────────────────────────

/// Cube · 3d wireframe — 40-unit SVG path baked as line segments.
/// Outer hexagon + inner Y-fork (top-back face) + vertical centre line.
const CUBE_OUTER: &[(f64, f64)] = &[
    (20.0, 4.0), (34.0, 12.0), (34.0, 28.0),
    (20.0, 36.0), (6.0, 28.0), (6.0, 12.0), (20.0, 4.0),
];
const CUBE_INNER: &[&[(f64, f64)]] = &[
    &[(6.0, 12.0), (20.0, 20.0), (34.0, 12.0)],
    &[(20.0, 20.0), (20.0, 36.0)],
];

fn draw_cube(ctx: &mut TinySkiaCpuRenderContext, cx: f64, cy: f64, target: f64) {
    // The reference SVG uses a 40-unit viewBox; scale + translate around (cx,cy).
    let scale = target / 40.0;
    let map = |p: &(f64, f64)| (cx + (p.0 - 20.0) * scale, cy + (p.1 - 20.0) * scale);

    ctx.set_stroke_color(ACCENT);
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

    ctx.set_global_alpha(0.7);
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
    ctx.set_global_alpha(1.0);
}

fn draw_titlebar(
    ctx: &mut TinySkiaCpuRenderContext,
    min_hovered: bool,
    close_hovered: bool,
) {
    // No background fill — the dot-grid shows through.

    // Cube logo, top-left.
    let logo_size = 26.0;
    draw_cube(ctx, 14.0 + logo_size / 2.0, TITLEBAR_H / 2.0, logo_size);

    // Right-side window controls.
    let close_x = W - TITLEBAR_BTN_W;
    let min_x = W - TITLEBAR_BTN_W * 2.0;
    let cy = TITLEBAR_H / 2.0;

    // Hover backgrounds.
    if min_hovered {
        ctx.set_fill_color("#ffffff14"); // 8% white
        ctx.fill_rect(min_x, 0.0, TITLEBAR_BTN_W, TITLEBAR_H);
    }
    if close_hovered {
        ctx.set_fill_color("#E0565699"); // red 60%
        ctx.fill_rect(close_x, 0.0, TITLEBAR_BTN_W, TITLEBAR_H);
    }

    // Minimise (–)
    ctx.set_stroke_color(if min_hovered { FG0 } else { FG3 });
    ctx.set_stroke_width(1.2);
    ctx.begin_path();
    ctx.move_to(min_x + TITLEBAR_BTN_W / 2.0 - 6.0, cy);
    ctx.line_to(min_x + TITLEBAR_BTN_W / 2.0 + 6.0, cy);
    ctx.stroke();

    // Close (×)
    ctx.set_stroke_color(if close_hovered { FG0 } else { FG3 });
    ctx.set_stroke_width(1.2);
    let cxh = close_x + TITLEBAR_BTN_W / 2.0;
    ctx.begin_path();
    ctx.move_to(cxh - 5.0, cy - 5.0);
    ctx.line_to(cxh + 5.0, cy + 5.0);
    ctx.move_to(cxh + 5.0, cy - 5.0);
    ctx.line_to(cxh - 5.0, cy + 5.0);
    ctx.stroke();

    // Subtle separator line under the titlebar.
    ctx.set_stroke_color("#ffffff10");
    ctx.set_stroke_width(1.0);
    ctx.begin_path();
    ctx.move_to(0.0, TITLEBAR_H);
    ctx.line_to(W, TITLEBAR_H);
    ctx.stroke();
}

// ── Hit region for the click target depends on state ─────────────────────────

fn click_target_rect(state: &LauncherState) -> uzor::Rect {
    match state {
        // On: anywhere below the titlebar = stop.
        LauncherState::On { .. } => uzor::Rect::new(0.0, TITLEBAR_H, W, H - TITLEBAR_H),
        // Off / Connecting / Error: cube + a generous click slop around it.
        _ => {
            let cx = W / 2.0;
            let cy = (H + TITLEBAR_H) / 2.0;
            let r = CUBE_OFF_SIZE; // hit area roughly 2× the cube — easy to tap
            uzor::Rect::new(cx - r, cy - r, r * 2.0, r * 2.0)
        }
    }
}

// ── winit ApplicationHandler ─────────────────────────────────────────────────

impl ApplicationHandler for LauncherHandler {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.app.is_some() {
            return;
        }

        let icon = load_icon();

        let mut attrs = Window::default_attributes()
            .with_title("Mirage")
            .with_inner_size(LogicalSize::new(W as u32, H as u32))
            .with_resizable(false)
            .with_decorations(false);  // chromeless — we draw our own titlebar
        if let Some(ic) = icon {
            attrs = attrs.with_window_icon(Some(ic));
        }

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("window creation never fails on supported platforms"),
        );

        let context = softbuffer::Context::new(window.clone())
            .expect("softbuffer context init always succeeds on Windows");
        let surface = softbuffer::Surface::new(&context, window.clone())
            .expect("softbuffer surface init always succeeds on Windows");

        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let now = Instant::now();

        self.app = Some(LauncherApp {
            window,
            surface,
            mouse: MouseAccum::default(),
            last_frame: now,
            startup: now,
            state: LauncherState::Off,
            children: Arc::new(Mutex::new(None)),
            event_rx,
            event_tx,
            rt: self.rt.clone(),
            coordinator: uzor::InputCoordinator::new(),
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
                // Kill children synchronously so the OS reaps them before we exit.
                // Job Object also enforces kill-on-close as a safety net, but the
                // explicit blocking kill is cleaner and avoids stderr races.
                let pair_opt = app.children.lock().expect("not poisoned").take();
                if let Some(mut pair) = pair_opt {
                    app.rt.block_on(async move {
                        // Hard timeout so a hung child can't pin the exit forever.
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            kill_pair(&mut pair),
                        ).await;
                    });
                }
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
                            // Carry the radar phase from Connecting so the
                            // doppler keeps marching without a reset.
                            let anchor = match app.state {
                                LauncherState::Connecting { started_at_ms } => started_at_ms,
                                _ => t,
                            };
                            app.state = LauncherState::On { radar_anchor_ms: anchor };
                        }
                        LauncherEvent::Failed(msg) => {
                            *app.children.lock().expect("not poisoned") = None;
                            app.state = LauncherState::Error(msg);
                        }
                        LauncherEvent::Stopped => {
                            *app.children.lock().expect("not poisoned") = None;
                            app.state = LauncherState::Off;
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

                // Register interactive widgets BEFORE draw_frame so hover state
                // can drive their visuals on this same frame.
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
                    click_target_rect(&app.state),
                    uzor::input::Sense::CLICK,
                );

                // Resolve responses from PREVIOUS frame's begin_frame call —
                // this is fine since we re-register the same widgets with the
                // same ids each frame, the coordinator carries hover state.
                let responses = app.coordinator.end_frame();

                // Compute hover state for titlebar buttons, then redraw with it.
                let mut tb_hover = TitlebarHover::default();
                let mut tb_min_click = false;
                let mut tb_close_click = false;
                let mut tb_drag_press = false;
                for (id, resp) in &responses {
                    match id.0.as_str() {
                        "tb_min" => {
                            tb_hover.minimize = resp.hovered;
                            if resp.clicked { tb_min_click = true; }
                        }
                        "tb_close" => {
                            tb_hover.close = resp.hovered;
                            if resp.clicked { tb_close_click = true; }
                        }
                        "tb_drag" => {
                            // Use hover + button_down to detect a drag-press.
                            // drag_window() must be called on Pressed, before
                            // the OS releases the click — coordinator only
                            // surfaces `clicked` on Released which is too late.
                            // We track press in the MouseInput handler instead;
                            // see app.tb_drag_pressed flag below.
                            let _ = resp;
                        }
                        _ => {}
                    }
                }
                if tb_close_click {
                    let pair_opt = app.children.lock().expect("not poisoned").take();
                    if let Some(mut pair) = pair_opt {
                        app.rt.block_on(async move {
                            let _ = tokio::time::timeout(
                                std::time::Duration::from_secs(3),
                                kill_pair(&mut pair),
                            ).await;
                        });
                    }
                    event_loop.exit();
                    return;
                }
                if tb_min_click {
                    app.window.set_minimized(true);
                }
                let _ = tb_drag_press;

                let dpr = app.window.scale_factor();
                let mut ctx = TinySkiaCpuRenderContext::new(width, height, dpr);
                draw_frame(&mut ctx, &app.state, t, tb_hover);
                for (id, resp) in &responses {
                    if !resp.clicked || id.0 != "btn" {
                        continue;
                    }
                    match &app.state {
                        LauncherState::Off | LauncherState::Error(_) => {
                            app.state = LauncherState::Connecting { started_at_ms: t };
                            let tx = app.event_tx.clone();
                            let children_slot = app.children.clone();
                            app.rt.spawn(async move {
                                match spawn_pair().await {
                                    Ok(pair) => {
                                        // Verify both alive after warmup.
                                        let cli_alive = pair.cli.id().is_some();
                                        let tun_alive = pair.tun.id().is_some();
                                        let mut pair = pair;
                                        let cli_dead = pair.cli.try_wait().map(|s| s.is_some()).unwrap_or(true);
                                        let tun_dead = pair.tun.try_wait().map(|s| s.is_some()).unwrap_or(true);
                                        if !cli_alive || !tun_alive || cli_dead || tun_dead {
                                            kill_pair(&mut pair).await;
                                            let _ = tx.send(LauncherEvent::Failed("process exited immediately".into()));
                                            return;
                                        }
                                        // Stash pair so the main thread / Stop click can kill it.
                                        *children_slot.lock().expect("not poisoned") = Some(pair);
                                        let _ = tx.send(LauncherEvent::Started);
                                    }
                                    Err(e) => {
                                        let _ = tx.send(LauncherEvent::Failed(e.to_string()));
                                    }
                                }
                            });
                        }
                        LauncherState::On { .. } => {
                            let tx = app.event_tx.clone();
                            let children_slot = app.children.clone();
                            app.rt.spawn(async move {
                                let pair_opt = children_slot.lock().expect("not poisoned").take();
                                if let Some(mut pair) = pair_opt {
                                    kill_pair(&mut pair).await;
                                }
                                let _ = tx.send(LauncherEvent::Stopped);
                            });
                        }
                        LauncherState::Connecting { .. } => {}
                    }
                }

                // RGBA → 0x00RRGGBB
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

                // Drag region — hand off to OS on Left-press in the titlebar
                // strip outside the window-control buttons.  drag_window() must
                // be called during the press, before the OS-level mouse capture
                // ends — coordinator's `clicked` only fires on Release, which is
                // too late, so we do this hit-test ourselves here.
                if button == MouseButton::Left && state == ElementState::Pressed {
                    if let Some((x, y)) = app.mouse.pos {
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

// ── Entry ────────────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt::init();

    #[cfg(windows)]
    jobobj::init();

    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime always initializes"),
    );

    let event_loop = EventLoop::new().expect("winit event loop init always succeeds");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut handler = LauncherHandler { app: None, rt };
    event_loop.run_app(&mut handler).expect("event loop run always succeeds");
}
