//! Full UI rendering for mirage2light-app.
//!
//! Implements the Client view using the uzor RenderContext
//! API + direct TinySkia calls where needed (circles, arcs, dashed lines).

use std::f64::consts::{PI, TAU};

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor_backend_tiny_skia::TinySkiaCpuRenderContext;

use crate::app::AppState;
use crate::runtime::ProcessStatus;

// ─────────────────────────────────────────────────────────────────────────────
// Theme
// ─────────────────────────────────────────────────────────────────────────────

const BG:      &str = "#08090B";
const SURFACE: &str = "#14151A";
const RAISED:  &str = "#1C1D23";
const BORDER:  &str = "#ffffff12";   // rgba(255,255,255,0.07)
const BORDERS: &str = "#ffffff1f";   // rgba(255,255,255,0.12)
const FG0:     &str = "#F4F4F5";
const FG1:     &str = "#D7D8DB";
const FG3:     &str = "#555860";
const ACCENT:  &str = "#FBB26A";
const ACCENT_G:&str = "#FBB26A59"; // 35% alpha
const L1:      &str = "#6DB8F5";
const OK:      &str = "#5CB87A";
const WARN:    &str = "#E8A838";

/// Width in logical pixels.
const W: f64 = 780.0;
/// Height in logical pixels.
const H: f64 = 1400.0;

// ─────────────────────────────────────────────────────────────────────────────
// Public draw entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Draw a full frame into `ctx` and register interactive widgets with the coordinator.
pub fn draw_frame(
    ctx: &mut TinySkiaCpuRenderContext,
    state: &mut AppState,
) {
    let t = state.animation_time; // milliseconds since start

    // ── background ──────────────────────────────────────────────────────────
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, W, H);

    // Dot-grid overlay (subtle)
    draw_dot_grid(ctx);

    // ── top strip ───────────────────────────────────────────────────────────
    draw_top_strip(ctx, &state.status);

    // ── mode tabs ───────────────────────────────────────────────────────────
    draw_mode_tabs(ctx);

    // ── mesh rings visualization ─────────────────────────────────────────────
    let cx = W / 2.0;
    let mesh_cy = 66.0 + 170.0; // center of the 340-high ring area
    draw_mesh_rings(ctx, cx, mesh_cy, &state.status, t);

    // ── connect button ───────────────────────────────────────────────────────
    let btn_y = mesh_cy + 170.0 + 16.0;
    draw_connect_button(ctx, cx, btn_y, &state.status);

    // Register connect button with coordinator
    let bw = 220.0;
    let bh = 60.0;
    state.coordinator.register(
        "connect_btn",
        uzor::Rect::new(cx - bw / 2.0, btn_y, bw, bh),
        uzor::input::Sense::CLICK,
    );

    // ── route line ───────────────────────────────────────────────────────────
    let route_y = btn_y + 60.0 + 28.0;
    draw_route_line(ctx, route_y, &state.status);

    // ── live stats ───────────────────────────────────────────────────────────
    let stats_y = route_y + 58.0;
    draw_live_stats(ctx, stats_y, &state.status, t);

    // ── selectors ────────────────────────────────────────────────────────────
    let sel_y = stats_y + 58.0;
    draw_selectors(ctx, sel_y, state);

    // Register selector hit regions
    let layout = selector_layout(sel_y);
    let item_x_start = 90.0;
    let sel_h = 20.0;
    let prefixes = ["sel_chain_", "sel_camo_", "sel_shaping_", "sel_client_"];
    for (gi, group) in layout.groups.iter().enumerate() {
        let item_spacing = (W - item_x_start - 10.0) / group.items.len() as f64;
        for ii in 0..group.items.len() {
            let ix = item_x_start + ii as f64 * item_spacing;
            let widget_id = format!("{}{}", prefixes[gi], ii);
            state.coordinator.register(
                widget_id,
                uzor::Rect::new(ix, group.y, item_spacing, sel_h),
                uzor::input::Sense::CLICK,
            );
        }
    }

    // ── bottom bar ───────────────────────────────────────────────────────────
    draw_bottom_bar(ctx);
}

// ─────────────────────────────────────────────────────────────────────────────
// Dot-grid background
// ─────────────────────────────────────────────────────────────────────────────

fn draw_dot_grid(ctx: &mut TinySkiaCpuRenderContext) {
    let step = 20.0_f64;
    ctx.set_fill_color("#ffffff09");
    let mut y = 0.0_f64;
    while y < H {
        let mut x = 0.0_f64;
        while x < W {
            ctx.begin_path();
            ctx.arc(x, y, 0.6, 0.0, TAU);
            ctx.fill();
            x += step;
        }
        y += step;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Top strip
// ─────────────────────────────────────────────────────────────────────────────

fn draw_top_strip(ctx: &mut TinySkiaCpuRenderContext, status: &ProcessStatus) {
    let y = 14.0;

    // Mirage logo mark (simplified triangle/chevron shape)
    ctx.set_stroke_color(ACCENT);
    ctx.set_stroke_width(1.75);
    ctx.set_line_join("round");
    ctx.begin_path();
    ctx.move_to(22.0, y + 14.0);
    ctx.line_to(31.0, y + 1.0);
    ctx.line_to(40.0, y + 14.0);
    ctx.move_to(26.0, y + 14.0);
    ctx.line_to(31.0, y + 7.0);
    ctx.line_to(36.0, y + 14.0);
    ctx.stroke();

    // "mirage" wordmark
    ctx.set_fill_color(ACCENT);
    ctx.set_font("bold 18px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("mirage", 44.0, y);

    // Session id
    ctx.set_fill_color(FG3);
    ctx.set_font("10px jetbrains mono");
    ctx.set_text_align(TextAlign::Right);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("SES · 7F2A", W - 58.0, y + 4.0);

    // Status dot+text
    let (dot, label) = match status {
        ProcessStatus::Running  => (OK,   "● ON"),
        ProcessStatus::Starting => (WARN, "○ NEG"),
        _                       => (FG3,  "· OFF"),
    };
    ctx.set_fill_color(dot);
    ctx.set_text_align(TextAlign::Right);
    ctx.fill_text(label, W - 16.0, y + 4.0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Mode tabs
// ─────────────────────────────────────────────────────────────────────────────

fn draw_mode_tabs(ctx: &mut TinySkiaCpuRenderContext) {
    let tab_y  = 48.0;
    let tab_h  = 30.0;
    let margin = 22.0;
    let total  = W - margin * 2.0;

    // Container
    ctx.set_stroke_color(BORDER);
    ctx.set_stroke_width(1.0);
    ctx.set_fill_color(BG);
    ctx.fill_rounded_rect(margin, tab_y, total, tab_h, 4.0);
    ctx.stroke_rounded_rect(margin, tab_y, total, tab_h, 4.0);

    // "Client" tab (active)
    ctx.set_fill_color(RAISED);
    ctx.fill_rounded_rect(margin + 3.0, tab_y + 3.0, (total - 6.0) / 2.0, tab_h - 6.0, 3.0);
    ctx.set_fill_color(FG0);
    ctx.set_font("bold 10px jetbrains mono");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("CLIENT", margin + (total / 4.0), tab_y + tab_h / 2.0);

    // "Admin" tab (inactive)
    ctx.set_fill_color(FG3);
    ctx.fill_text("ADMIN", margin + (total * 3.0 / 4.0), tab_y + tab_h / 2.0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Mesh rings
// ─────────────────────────────────────────────────────────────────────────────

fn draw_mesh_rings(
    ctx: &mut TinySkiaCpuRenderContext,
    cx: f64,
    cy: f64,
    status: &ProcessStatus,
    t: f64,
) {
    let r_outer = 140.0_f64;
    let r_l1    = 92.0_f64;

    let active = !matches!(status, ProcessStatus::Stopped);

    // color palette
    let node_color = match status {
        ProcessStatus::Running  => ACCENT,
        ProcessStatus::Starting => WARN,
        _                       => FG3,
    };
    let glow_color = match status {
        ProcessStatus::Running  => ACCENT_G,
        ProcessStatus::Starting => "#E8A83833",
        _                       => FG3,
    };

    // hot node indices
    let hot_l2: i32 = if active { (t / 800.0) as i32 % 6 } else { -1 };
    let hot_l1: i32 = if active { (t / 1200.0) as i32 % 3 } else { -1 };

    // L2 node positions
    let l2_nodes: Vec<(f64, f64)> = (0..6)
        .map(|i| {
            let a = (i as f64 / 6.0) * TAU - PI / 2.0;
            (cx + a.cos() * r_outer, cy + a.sin() * r_outer)
        })
        .collect();

    // L1 node positions
    let l1_nodes: Vec<(f64, f64)> = (0..3)
        .map(|i| {
            let a = (i as f64 / 3.0) * TAU + PI / 2.0;
            (cx + a.cos() * r_l1, cy + a.sin() * r_l1)
        })
        .collect();

    // ── halo rings when active ───────────────────────────────────────────────
    if active {
        for offset in [0.0_f64, 15.0] {
            let phase = (t / 20.0 + offset) % 30.0;
            let r = r_outer + phase;
            let alpha = 1.0 - phase / 30.0;
            ctx.set_global_alpha(alpha);
            ctx.set_stroke_color(glow_color);
            ctx.set_stroke_width(0.5);
            ctx.begin_path();
            ctx.arc(cx, cy, r, 0.0, TAU);
            ctx.stroke();
        }
        ctx.set_global_alpha(1.0);
    }

    // ── scaffold rings ───────────────────────────────────────────────────────
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

    // ── tick marks around outer ring ─────────────────────────────────────────
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

    // ── connection lines when active ─────────────────────────────────────────
    if active {
        // user → L1
        for (i, &(lx, ly)) in l1_nodes.iter().enumerate() {
            let is_hot = i as i32 == hot_l1;
            let (color, width, dash) = if is_hot {
                (L1, 1.4_f64, vec![])
            } else {
                ("#6DB8F540", 0.8, vec![2.0, 3.0])
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

        // L1 → L2 (nearest L1 for each L2)
        for (i, &(x2, y2)) in l2_nodes.iter().enumerate() {
            let nearest = l1_nodes
                .iter()
                .copied()
                .min_by(|&(ax, ay), &(bx, by)| {
                    let da = (ax - x2).hypot(ay - y2);
                    let db = (bx - x2).hypot(by - y2);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or((cx, cy));

            let is_hot = i as i32 == hot_l2;
            let (color, width, dash) = if is_hot {
                (node_color, 1.4_f64, vec![])
            } else {
                ("#FBB26A38", 0.8, vec![2.0, 3.0])
            };
            ctx.set_stroke_color(color);
            ctx.set_stroke_width(width);
            ctx.set_line_dash(&dash);
            ctx.begin_path();
            ctx.move_to(nearest.0, nearest.1);
            ctx.line_to(x2, y2);
            ctx.stroke();
        }
        ctx.set_line_dash(&[]);
    }

    // ── L2 nodes ─────────────────────────────────────────────────────────────
    for (i, &(nx, ny)) in l2_nodes.iter().enumerate() {
        let is_hot = i as i32 == hot_l2;
        let border = if is_hot { node_color } else { BORDERS };
        let dot    = if is_hot { node_color } else { FG3 };
        let bw     = if is_hot { 1.6 } else { 1.0 };

        // outer halo
        if is_hot {
            ctx.set_global_alpha(0.5);
            ctx.set_stroke_color(node_color);
            ctx.set_stroke_width(1.0);
            ctx.begin_path();
            ctx.arc(nx, ny, 14.0, 0.0, TAU);
            ctx.stroke();
            ctx.set_global_alpha(1.0);
        }

        // circle body
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

    // ── L1 nodes (squares) ────────────────────────────────────────────────────
    for (i, &(nx, ny)) in l1_nodes.iter().enumerate() {
        let is_hot = i as i32 == hot_l1;
        let border = if is_hot { L1 } else { BORDERS };
        let dot    = if is_hot { L1 } else { FG3 };
        let bw     = if is_hot { 1.6 } else { 1.0 };

        // square body
        ctx.set_fill_color(SURFACE);
        ctx.fill_rect(nx - 7.0, ny - 7.0, 14.0, 14.0);

        ctx.set_stroke_color(border);
        ctx.set_stroke_width(bw);
        ctx.stroke_rect(nx - 7.0, ny - 7.0, 14.0, 14.0);

        // center dot
        ctx.set_fill_color(dot);
        ctx.begin_path();
        ctx.arc(nx, ny, 2.0, 0.0, TAU);
        ctx.fill();
    }

    // ── center user dot ───────────────────────────────────────────────────────
    let user_color = match status {
        ProcessStatus::Running  => ACCENT,
        ProcessStatus::Starting => WARN,
        _                       => FG3,
    };

    if active {
        let phase = (t / 100.0) % 6.0;
        let ring_r = 10.0 + phase;
        let ring_a = 1.0 - phase / 6.0;
        ctx.set_global_alpha(ring_a);
        ctx.set_stroke_color(user_color);
        ctx.set_stroke_width(1.0);
        ctx.begin_path();
        ctx.arc(cx, cy, ring_r, 0.0, TAU);
        ctx.stroke();
        ctx.set_global_alpha(1.0);
    }

    ctx.set_fill_color(user_color);
    ctx.begin_path();
    ctx.arc(cx, cy, 6.0, 0.0, TAU);
    ctx.fill();
}

// ─────────────────────────────────────────────────────────────────────────────
// Connect button
// ─────────────────────────────────────────────────────────────────────────────

fn draw_connect_button(
    ctx: &mut TinySkiaCpuRenderContext,
    cx: f64,
    top: f64,
    status: &ProcessStatus,
) {
    let bw = 220.0;
    let bh = 60.0;
    let bx = cx - bw / 2.0;

    let (label, sub, bg, fg, border) = match status {
        ProcessStatus::Running => (
            "DISCONNECT", "tap to stop",
            ACCENT, "#1A1400", ACCENT,
        ),
        ProcessStatus::Starting => (
            "HANDSHAKE\u{2026}", "negotiating route",
            RAISED, WARN, "#E8A83880",
        ),
        _ => (
            "CONNECT", "tap to route",
            SURFACE, FG0, BORDERS,
        ),
    };

    // Glow when Running
    if matches!(status, ProcessStatus::Running) {
        for r in [1u32, 2, 3] {
            let expand = r as f64 * 4.0;
            ctx.set_global_alpha(0.15 / r as f64);
            ctx.set_fill_color(ACCENT);
            ctx.fill_rounded_rect(
                bx - expand,
                top - expand,
                bw + expand * 2.0,
                bh + expand * 2.0,
                4.0 + expand,
            );
        }
        ctx.set_global_alpha(1.0);
    }

    // Background
    ctx.set_fill_color(bg);
    ctx.fill_rounded_rect(bx, top, bw, bh, 4.0);

    // Border
    ctx.set_stroke_color(border);
    ctx.set_stroke_width(1.5);
    ctx.stroke_rounded_rect(bx, top, bw, bh, 4.0);

    // Label
    ctx.set_fill_color(fg);
    ctx.set_font("bold 14px jetbrains mono");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(label, cx, top + bh / 2.0);

    // Sub label
    ctx.set_fill_color(FG3);
    ctx.set_font("10px jetbrains mono");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(sub, cx, top + bh + 8.0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Route line
// ─────────────────────────────────────────────────────────────────────────────

fn draw_route_line(ctx: &mut TinySkiaCpuRenderContext, top: f64, status: &ProcessStatus) {
    let active = matches!(status, ProcessStatus::Running);
    let margin = 24.0;
    let h = 50.0;

    // Top border
    ctx.set_stroke_color(BORDER);
    ctx.set_stroke_width(1.0);
    ctx.begin_path();
    ctx.move_to(0.0, top);
    ctx.line_to(W, top);
    ctx.stroke();

    let col_w = (W - margin * 2.0) / 3.0;

    struct Col {
        label: &'static str,
        value: &'static str,
        color: &'static str,
    }

    let cols = [
        Col { label: "ORIGIN", value: "RU-MSK",       color: FG1 },
        Col { label: "L1",     value: "YANDEX-MSK-A", color: L1  },
        Col { label: "L2",     value: "TIMEWEB-AMS",  color: ACCENT },
    ];

    for (i, col) in cols.iter().enumerate() {
        let x = margin + i as f64 * col_w;

        ctx.set_fill_color(FG3);
        ctx.set_font("9px jetbrains mono");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(col.label, x, top + 12.0);

        let val_color = if active { col.color } else { FG3 };
        let val_text  = if active { col.value } else { "\u{2014} \u{2014} \u{2014}" };
        ctx.set_fill_color(val_color);
        ctx.set_font("bold 12px jetbrains mono");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(val_text, x, top + 24.0);

        // Arrow between columns
        if i < 2 {
            ctx.set_fill_color(FG3);
            ctx.set_font("11px jetbrains mono");
            ctx.set_text_align(TextAlign::Left);
            ctx.fill_text("\u{2192}", x + col_w - 12.0, top + 24.0);
        }
    }

    let _ = h;
}

// ─────────────────────────────────────────────────────────────────────────────
// Live stats
// ─────────────────────────────────────────────────────────────────────────────

fn draw_live_stats(
    ctx: &mut TinySkiaCpuRenderContext,
    top: f64,
    status: &ProcessStatus,
    t: f64,
) {
    let margin = 24.0;
    let h = 56.0;

    // Borders
    ctx.set_stroke_color(BORDER);
    ctx.set_stroke_width(1.0);
    for border_y in [top, top + h] {
        ctx.begin_path();
        ctx.move_to(0.0, border_y);
        ctx.line_to(W, border_y);
        ctx.stroke();
    }

    let col_w = (W - margin * 2.0) / 3.0;

    struct Stat {
        label: &'static str,
        unit: &'static str,
    }

    let stats = [
        Stat { label: "PING", unit: "ms"   },
        Stat { label: "DOWN", unit: "KB/s" },
        Stat { label: "UP",   unit: "KB/s" },
    ];

    for (i, stat) in stats.iter().enumerate() {
        let x = margin + i as f64 * col_w;

        ctx.set_fill_color(FG3);
        ctx.set_font("9px jetbrains mono");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(stat.label, x, top + 10.0);

        let (val_str, val_color) = match (status, i) {
            (ProcessStatus::Running, 0) => {
                let v = 18.0 + (t / 400.0).sin() * 4.0;
                (format!("{:.0}", v), OK)
            }
            (ProcessStatus::Running, 1) => {
                let v = 312.0 + (t / 500.0).cos() * 78.0;
                (format!("{:.0}", v), FG0)
            }
            (ProcessStatus::Running, 2) => {
                let v = 84.0 + (t / 600.0).sin() * 22.0;
                (format!("{:.0}", v), FG0)
            }
            (ProcessStatus::Starting, 0) => {
                let v = ((t / 100.0) as u64 % 99) as f64;
                (format!("{:.0}", v), WARN)
            }
            _ => ("\u{2014}\u{2014}".to_string(), FG3),
        };

        let (num_text, unit_text) = if matches!(status, ProcessStatus::Running)
            || (i == 0 && matches!(status, ProcessStatus::Starting))
        {
            (val_str, stat.unit)
        } else {
            (val_str, "")
        };

        ctx.set_fill_color(val_color);
        ctx.set_font("bold 18px jetbrains mono");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(&num_text, x, top + 24.0);

        if !unit_text.is_empty() {
            let num_w = ctx.measure_text(&num_text);
            ctx.set_fill_color(FG3);
            ctx.set_font("11px jetbrains mono");
            ctx.fill_text(unit_text, x + num_w + 2.0, top + 30.0);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Radio selector rows
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the y-coords and rects for each selector group.
/// Used both by draw and hit-testing.
pub struct SelectorLayout {
    pub groups: Vec<SelectorGroup>,
}

pub struct SelectorGroup {
    pub label: &'static str,
    pub y: f64,
    pub items: &'static [&'static str],
}

fn selector_layout(top: f64) -> SelectorLayout {
    SelectorLayout {
        groups: vec![
            SelectorGroup { label: "CHAIN",      y: top,          items: &["CLASSIC-DIRECT","BYTE-CIPHER","CRYPTO-RELAY","THREE-HOP-ONION","TELESCOPIC"] },
            SelectorGroup { label: "CAMOUFLAGE", y: top + 22.0,   items: &["RAW","JSON-API","GRPC","H2DATA"] },
            SelectorGroup { label: "SHAPING",    y: top + 44.0,   items: &["NONE","WEB-BROWSE","CLOUD-API","STREAMING"] },
            SelectorGroup { label: "CLIENT MODE",y: top + 66.0,   items: &["SOCKS5","HTTP-CONNECT","TUN"] },
        ],
    }
}

fn draw_selectors(ctx: &mut TinySkiaCpuRenderContext, top: f64, state: &AppState) {
    let layout = selector_layout(top);

    let selected = [
        state.sel_chain,
        state.sel_camouflage,
        state.sel_shaping,
        state.sel_client_mode,
    ];

    for (gi, group) in layout.groups.iter().enumerate() {
        // Group label
        ctx.set_fill_color(FG3);
        ctx.set_font("9px jetbrains mono");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(group.label, 14.0, group.y + 10.0);

        // Items
        let item_x_start = 90.0;
        let item_spacing = (W - item_x_start - 10.0) / group.items.len() as f64;

        for (ii, &item_label) in group.items.iter().enumerate() {
            let ix = item_x_start + ii as f64 * item_spacing;
            let is_sel = ii == selected[gi];

            // Radio dot
            let dot_color = if is_sel { ACCENT } else { FG3 };
            ctx.set_fill_color(dot_color);
            ctx.begin_path();
            ctx.arc(ix + 5.0, group.y + 10.0, 3.5, 0.0, TAU);
            ctx.fill();
            if is_sel {
                ctx.set_stroke_color(ACCENT);
                ctx.set_stroke_width(1.0);
                ctx.begin_path();
                ctx.arc(ix + 5.0, group.y + 10.0, 6.0, 0.0, TAU);
                ctx.stroke();
            }

            // Label text
            ctx.set_fill_color(if is_sel { FG1 } else { FG3 });
            ctx.set_font("8px jetbrains mono");
            ctx.set_text_align(TextAlign::Left);
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.fill_text(item_label, ix + 13.0, group.y + 10.0);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Bottom bar
// ─────────────────────────────────────────────────────────────────────────────

fn draw_bottom_bar(ctx: &mut TinySkiaCpuRenderContext) {
    let bar_y = H - 36.0;

    ctx.set_stroke_color(BORDER);
    ctx.set_stroke_width(1.0);
    ctx.begin_path();
    ctx.move_to(0.0, bar_y);
    ctx.line_to(W, bar_y);
    ctx.stroke();

    ctx.set_fill_color(FG3);
    ctx.set_font("10px jetbrains mono");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("MESH · EU-PROD-01", 24.0, bar_y + 18.0);

    ctx.set_text_align(TextAlign::Right);
    ctx.fill_text("V 2.4.1 · BUILD 2814", W - 16.0, bar_y + 18.0);
}

