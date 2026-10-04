//! The menu — a resizable, scrollable, TABBED container (the client's control
//! plane). Covers ~half the window by default; drag the bottom separator to
//! resize; content scrolls (wheel) inside a clipped viewport.
//!
//! Container mechanics (panel / tabbar / content-clip / separator / scrollbar)
//! are independent of content. Tabs: Connection · Nodes · Style · Setup.

use uzor::core::types::Rect;
use uzor::render::{RenderContext, TextAlign, TextBaseline};

use crate::centerpiece::{ConnPhase, SkinKind};
use crate::nodes::CATALOG;

pub const MENU_TOP: f64 = 32.0;
pub const TABBAR_H: f64 = 32.0;
pub const SEP_H: f64 = 8.0;
/// Default panel height as a fraction of the available area.
pub const DEFAULT_FRAC: f64 = 0.5;
pub const MIN_FRAC: f64 = 0.22;
pub const MAX_FRAC: f64 = 0.96;

// ── Palette ──────────────────────────────────────────────────────────────────────
const BG: &str = "rgba(14,14,17,0.98)";
const SCRIM: &str = "rgba(0,0,0,0.5)";
const AMBER: &str = "#FBB26A";
const FG: &str = "#F4F4F5";
const MUTED: &str = "#878B91";
const DIMC: &str = "#6B6F7A";
const GREEN: &str = "#7FD6A0";
const LINE: &str = "rgba(255,255,255,0.08)";

// ── Tabs ───────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuTab {
    Connection,
    Nodes,
    Style,
    Setup,
}

impl MenuTab {
    /// Every tab, in tab-bar order.
    pub const ALL: [MenuTab; 4] = [MenuTab::Connection, MenuTab::Nodes, MenuTab::Style, MenuTab::Setup];

    pub fn label(self) -> &'static str {
        match self {
            MenuTab::Connection => "Connect",
            MenuTab::Nodes => "Nodes",
            MenuTab::Style => "Style",
            MenuTab::Setup => "Setup",
        }
    }
}

// ── Content rows ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub enum MenuRow {
    Status,
    /// Exit-node mode segmented control: Auto | Manual.
    ExitMode,
    /// An exit node (index into nodes::CATALOG).
    Node(usize),
    Skin(SkinKind),
    Action(MenuAction),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    ImportFile,
    ImportText,
    ImportQrClip,
    ImportCamera,
}

pub struct LaidRow {
    pub row: MenuRow,
    /// Content-local Y (before scroll).
    pub y: f64,
    pub h: f64,
    pub clickable: bool,
}

fn metrics(row: &MenuRow) -> (f64, bool) {
    match row {
        MenuRow::Status => (52.0, false),
        MenuRow::ExitMode => (46.0, true),
        MenuRow::Node(_) => (34.0, true),
        MenuRow::Skin(_) => (38.0, true),
        MenuRow::Action(_) => (40.0, true),
    }
}

/// Rows for the active tab. `exit_auto` controls whether the Nodes tab shows
/// the manual exit-node picker.
pub fn tab_rows(tab: MenuTab, exit_auto: bool) -> Vec<MenuRow> {
    match tab {
        MenuTab::Connection => vec![MenuRow::Status],
        MenuTab::Nodes => {
            let mut rows = vec![MenuRow::ExitMode];
            if !exit_auto {
                rows.extend((0..CATALOG.len()).map(MenuRow::Node));
            }
            rows
        }
        MenuTab::Style => [SkinKind::Bloom, SkinKind::Planet, SkinKind::Radar, SkinKind::Aurora]
            .into_iter()
            .map(MenuRow::Skin)
            .collect(),
        MenuTab::Setup => vec![
            MenuRow::Action(MenuAction::ImportFile),
            MenuRow::Action(MenuAction::ImportText),
            MenuRow::Action(MenuAction::ImportQrClip),
            MenuRow::Action(MenuAction::ImportCamera),
        ],
    }
}

/// Lay out content rows (content-local Y from 0); returns rows + total height.
pub fn layout_rows(rows: &[MenuRow]) -> (Vec<LaidRow>, f64) {
    let mut y = 6.0;
    let mut out = Vec::with_capacity(rows.len());
    for &row in rows {
        let (h, clickable) = metrics(&row);
        out.push(LaidRow { row, y, h, clickable });
        y += h;
    }
    (out, y + 6.0)
}

// ── Geometry ─────────────────────────────────────────────────────────────────────

pub struct Geom {
    pub panel: Rect,
    pub tabbar: Rect,
    pub content: Rect,
    pub sep: Rect,
    pub tabs: Vec<Rect>,
}

/// Compute container rects for a panel of height `panel_h` (px, below chrome)
/// with `ntabs` equal-width tab columns.
pub fn geom(w: f64, h: f64, panel_h: f64, ntabs: usize) -> Geom {
    let panel = Rect::new(0.0, MENU_TOP, w, panel_h);
    let tabbar = Rect::new(0.0, MENU_TOP, w, TABBAR_H);
    let content = Rect::new(
        0.0,
        MENU_TOP + TABBAR_H,
        w,
        (panel_h - TABBAR_H - SEP_H).max(0.0),
    );
    let sep = Rect::new(0.0, MENU_TOP + panel_h - SEP_H, w, SEP_H);
    let n = ntabs.max(1);
    let tw = w / n as f64;
    let tabs = (0..n)
        .map(|i| Rect::new(tw * i as f64, MENU_TOP, tw, TABBAR_H))
        .collect();
    let _ = h;
    Geom { panel, tabbar, content, sep, tabs }
}

// ── Draw ───────────────────────────────────────────────────────────────────────

fn phase_label(p: ConnPhase) -> (&'static str, &'static str) {
    match p {
        ConnPhase::Disconnected => ("OFF", DIMC),
        ConnPhase::Connecting => ("CONNECTING", AMBER),
        ConnPhase::Connected => ("CONNECTED", GREEN),
        ConnPhase::Disconnecting => ("DISCONNECTING", AMBER),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn draw_menu(
    ctx: &mut dyn RenderContext,
    w: f64,
    h: f64,
    g: &Geom,
    tab: MenuTab,
    tabs: &[MenuTab],
    laid: &[LaidRow],
    content_h: f64,
    scroll: f64,
    skin: SkinKind,
    selected: Option<usize>,
    exit_auto: bool,
    phase: ConnPhase,
    note: Option<&str>,
) {
    // Scrim over the rest of the app.
    ctx.set_fill_color(SCRIM);
    ctx.fill_rect(0.0, MENU_TOP, w, (h - MENU_TOP).max(0.0));

    // Panel.
    ctx.set_fill_color(BG);
    ctx.fill_rect(g.panel.x, g.panel.y, g.panel.width, g.panel.height);
    ctx.set_fill_color(AMBER);
    ctx.fill_rect(g.panel.x, g.panel.y, 2.0, g.panel.height);

    // ── Tab bar ──────────────────────────────────────────────────────────────
    for (i, tb) in tabs.iter().enumerate() {
        let r = g.tabs[i];
        let active = *tb == tab;
        if active {
            ctx.set_fill_color("rgba(251,178,106,0.12)");
            ctx.fill_rect(r.x, r.y, r.width, r.height);
            ctx.set_fill_color(AMBER);
            ctx.fill_rect(r.x, r.y + r.height - 2.0, r.width, 2.0);
        }
        ctx.set_fill_color(if active { AMBER } else { MUTED });
        ctx.set_font("12px sans-serif");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(tb.label(), r.x + r.width / 2.0, r.y + r.height / 2.0);
    }
    ctx.set_fill_color(LINE);
    ctx.fill_rect(0.0, g.tabbar.y + g.tabbar.height - 1.0, w, 1.0);

    // ── Content (clipped + scrolled) ───────────────────────────────────────────
    ctx.save();
    ctx.clip_rect(g.content.x, g.content.y, g.content.width, g.content.height);
    let cy = g.content.y - scroll;
    for lr in laid {
        let ry = cy + lr.y;
        if ry + lr.h < g.content.y || ry > g.content.y + g.content.height {
            continue; // culled
        }
        draw_row(ctx, w, lr, ry, skin, selected, exit_auto, phase);
    }
    ctx.restore();

    // ── Scrollbar ──────────────────────────────────────────────────────────────
    if content_h > g.content.height {
        let track_h = g.content.height;
        let thumb_h = (track_h * (track_h / content_h)).max(24.0);
        let max_scroll = (content_h - track_h).max(1.0);
        let ty = g.content.y + (track_h - thumb_h) * (scroll / max_scroll).clamp(0.0, 1.0);
        ctx.set_fill_color("rgba(255,255,255,0.14)");
        ctx.fill_rect(w - 4.0, ty, 3.0, thumb_h);
    }

    // ── Separator handle ────────────────────────────────────────────────────────
    ctx.set_fill_color("rgba(255,255,255,0.06)");
    ctx.fill_rect(g.sep.x, g.sep.y, g.sep.width, g.sep.height);
    ctx.set_fill_color(DIMC);
    ctx.fill_rect(w / 2.0 - 14.0, g.sep.y + g.sep.height / 2.0 - 1.0, 28.0, 2.0);

    if let Some(n) = note {
        ctx.set_fill_color(MUTED);
        ctx.set_font("11px monospace");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(n, w / 2.0, g.panel.y + g.panel.height - 12.0);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    ctx: &mut dyn RenderContext,
    w: f64,
    lr: &LaidRow,
    ry: f64,
    skin: SkinKind,
    selected: Option<usize>,
    exit_auto: bool,
    phase: ConnPhase,
) {
    match lr.row {
        MenuRow::Status => {
            let (lbl, col) = phase_label(phase);
            ctx.set_fill_color(MUTED);
            ctx.set_font("11px monospace");
            ctx.set_text_align(TextAlign::Left);
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.fill_text("status", 18.0, ry + 16.0);
            ctx.set_fill_color(col);
            ctx.set_text_align(TextAlign::Right);
            ctx.fill_text(lbl, w - 18.0, ry + 16.0);
            ctx.set_fill_color(MUTED);
            ctx.set_text_align(TextAlign::Left);
            ctx.fill_text("exit", 18.0, ry + 36.0);
            // Auto mode wins regardless of any stale manual pick.
            let exit = if exit_auto {
                "auto".to_string()
            } else {
                selected
                    .and_then(|i| CATALOG.get(i))
                    .map(|n| n.code.to_string())
                    .unwrap_or_else(|| "manual · не выбрана".to_string())
            };
            ctx.set_fill_color(FG);
            ctx.set_text_align(TextAlign::Right);
            ctx.fill_text(&exit, w - 18.0, ry + 36.0);
        }
        MenuRow::ExitMode => {
            ctx.set_fill_color(MUTED);
            ctx.set_font("12px sans-serif");
            ctx.set_text_align(TextAlign::Left);
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.fill_text("exit node", 18.0, ry + lr.h / 2.0);
            // Segmented pill: [ Auto | Manual ] right-aligned.
            let pill_w = 140.0;
            let pill_h = 26.0;
            let px = w - 18.0 - pill_w;
            let py = ry + (lr.h - pill_h) / 2.0;
            ctx.set_fill_color("rgba(255,255,255,0.05)");
            ctx.fill_rect(px, py, pill_w, pill_h);
            let half = pill_w / 2.0;
            let active_x = if exit_auto { px } else { px + half };
            ctx.set_fill_color("rgba(251,178,106,0.16)");
            ctx.fill_rect(active_x, py, half, pill_h);
            ctx.set_fill_color(AMBER);
            ctx.fill_rect(active_x, py + pill_h - 2.0, half, 2.0);
            ctx.set_font("11px sans-serif");
            ctx.set_text_align(TextAlign::Center);
            ctx.set_fill_color(if exit_auto { AMBER } else { MUTED });
            ctx.fill_text("Auto", px + half / 2.0, py + pill_h / 2.0);
            ctx.set_fill_color(if exit_auto { MUTED } else { AMBER });
            ctx.fill_text("Manual", px + half + half / 2.0, py + pill_h / 2.0);
        }
        MenuRow::Node(i) => {
            let Some(n) = CATALOG.get(i) else { return };
            let sel = selected == Some(i);
            if sel {
                ctx.set_fill_color("rgba(251,178,106,0.10)");
                ctx.fill_rect(2.0, ry, w - 2.0, lr.h);
            }
            ctx.set_fill_color(if sel { AMBER } else { FG });
            ctx.set_font("13px sans-serif");
            ctx.set_text_align(TextAlign::Left);
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.fill_text(n.code, 20.0, ry + lr.h / 2.0);
        }
        MenuRow::Skin(s) => {
            let active = s == skin;
            if active {
                ctx.set_fill_color("rgba(251,178,106,0.10)");
                ctx.fill_rect(2.0, ry, w - 2.0, lr.h);
            }
            ctx.set_fill_color(if active { AMBER } else { FG });
            ctx.set_font("14px sans-serif");
            ctx.set_text_align(TextAlign::Left);
            ctx.set_text_baseline(TextBaseline::Middle);
            let name = match s {
                SkinKind::Bloom => "Bloom",
                SkinKind::Planet => "Planet",
                SkinKind::Radar => "Radar",
                SkinKind::Aurora => "Aurora",
            };
            ctx.fill_text(name, 20.0, ry + lr.h / 2.0);
            if active {
                ctx.set_fill_color(AMBER);
                ctx.begin_path();
                ctx.arc(w - 22.0, ry + lr.h / 2.0, 3.5, 0.0, std::f64::consts::TAU);
                ctx.fill();
            }
        }
        MenuRow::Action(a) => {
            let label = match a {
                MenuAction::ImportFile => "Import  ·  from file",
                MenuAction::ImportText => "Import  ·  paste text",
                MenuAction::ImportQrClip => "Import  ·  paste QR",
                MenuAction::ImportCamera => "Import  ·  scan camera",
            };
            ctx.set_fill_color(AMBER);
            ctx.set_font("14px sans-serif");
            ctx.set_text_align(TextAlign::Left);
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.fill_text(label, 20.0, ry + lr.h / 2.0);
            ctx.set_fill_color(DIMC);
            ctx.set_text_align(TextAlign::Right);
            ctx.set_font("15px sans-serif");
            ctx.fill_text("›", w - 18.0, ry + lr.h / 2.0);
        }
    }
}
