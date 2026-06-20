//! The menu — the client's control plane (not just a skin switcher).
//!
//! A full-height top overlay with sections: CONNECTION (phase + chosen exit),
//! EXIT NODE (the fleet list, selectable — this is where radar/aurora/bloom get
//! their node picker since they don't draw nodes in-scene), APPEARANCE (skins),
//! SETUP (config import / QR, log in to the site). The model is data-driven:
//! `build_rows` + `layout` produce rows the shell both draws and hit-tests.

use uzor::render::{RenderContext, TextAlign, TextBaseline};

use crate::centerpiece::{ConnPhase, SkinKind};
use crate::nodes::FLEET;

/// Y where the overlay begins (below the chrome strip).
pub const MENU_TOP: f64 = 32.0;

// ── Palette ──────────────────────────────────────────────────────────────────────
const BG: &str = "rgba(14,14,17,0.98)";
const SCRIM: &str = "rgba(0,0,0,0.5)";
const AMBER: &str = "#FBB26A";
const FG: &str = "#F4F4F5";
const MUTED: &str = "#878B91";
const DIMC: &str = "#6B6F7A";
const GREEN: &str = "#7FD6A0";
const LINE: &str = "rgba(255,255,255,0.07)";

// ── Rows ───────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub enum MenuRow {
    Header(&'static str),
    Status,
    Node(usize),
    Skin(SkinKind),
    Action(MenuAction),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    ImportConfig,
    Login,
}

pub struct LaidRow {
    pub row: MenuRow,
    pub y: f64,
    pub h: f64,
    pub clickable: bool,
}

fn metrics(row: &MenuRow) -> (f64, bool) {
    match row {
        MenuRow::Header(_) => (24.0, false),
        MenuRow::Status => (40.0, false),
        MenuRow::Node(_) => (30.0, true),
        MenuRow::Skin(_) => (32.0, true),
        MenuRow::Action(_) => (34.0, true),
    }
}

/// Build the section row list.
pub fn build_rows() -> Vec<MenuRow> {
    let mut v = vec![MenuRow::Header("CONNECTION"), MenuRow::Status, MenuRow::Header("EXIT NODE")];
    for i in 0..FLEET.len() {
        v.push(MenuRow::Node(i));
    }
    v.push(MenuRow::Header("APPEARANCE"));
    for s in [SkinKind::Bloom, SkinKind::Planet, SkinKind::Radar, SkinKind::Aurora] {
        v.push(MenuRow::Skin(s));
    }
    v.push(MenuRow::Header("SETUP"));
    v.push(MenuRow::Action(MenuAction::ImportConfig));
    v.push(MenuRow::Action(MenuAction::Login));
    v
}

/// Lay out rows top-to-bottom; returns the laid rows + total content height.
pub fn layout(rows: &[MenuRow]) -> (Vec<LaidRow>, f64) {
    let mut y = MENU_TOP + 8.0;
    let mut out = Vec::with_capacity(rows.len());
    for &row in rows {
        let (h, clickable) = metrics(&row);
        out.push(LaidRow { row, y, h, clickable });
        y += h;
    }
    (out, y + 8.0)
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
    laid: &[LaidRow],
    skin: SkinKind,
    selected: Option<usize>,
    phase: ConnPhase,
    note: Option<&str>,
) {
    // Scrim + full-height panel with amber left accent.
    ctx.set_fill_color(SCRIM);
    ctx.fill_rect(0.0, MENU_TOP, w, (h - MENU_TOP).max(0.0));
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, MENU_TOP, w, (h - MENU_TOP).max(0.0));
    ctx.set_fill_color(AMBER);
    ctx.fill_rect(0.0, MENU_TOP, 2.0, (h - MENU_TOP).max(0.0));

    for lr in laid {
        match lr.row {
            MenuRow::Header(t) => {
                ctx.set_fill_color(AMBER);
                ctx.set_font("10px monospace");
                ctx.set_text_align(TextAlign::Left);
                ctx.set_text_baseline(TextBaseline::Middle);
                ctx.fill_text(t, 16.0, lr.y + lr.h - 9.0);
                ctx.set_fill_color(LINE);
                ctx.fill_rect(16.0, lr.y + lr.h - 1.0, w - 32.0, 1.0);
            }
            MenuRow::Status => {
                let (lbl, col) = phase_label(phase);
                ctx.set_fill_color(MUTED);
                ctx.set_font("11px monospace");
                ctx.set_text_align(TextAlign::Left);
                ctx.set_text_baseline(TextBaseline::Middle);
                ctx.fill_text("status", 18.0, lr.y + 13.0);
                ctx.set_fill_color(col);
                ctx.set_text_align(TextAlign::Right);
                ctx.fill_text(lbl, w - 18.0, lr.y + 13.0);

                ctx.set_fill_color(MUTED);
                ctx.set_text_align(TextAlign::Left);
                ctx.fill_text("exit", 18.0, lr.y + 29.0);
                let exit = selected.map(|i| {
                    let n = &FLEET[i];
                    format!("{} · {}", n.reg, n.city)
                });
                ctx.set_fill_color(FG);
                ctx.set_text_align(TextAlign::Right);
                ctx.fill_text(exit.as_deref().unwrap_or("auto"), w - 18.0, lr.y + 29.0);
            }
            MenuRow::Node(i) => {
                let n = &FLEET[i];
                let sel = selected == Some(i);
                if sel {
                    ctx.set_fill_color("rgba(251,178,106,0.10)");
                    ctx.fill_rect(2.0, lr.y, w - 2.0, lr.h);
                }
                ctx.set_fill_color(if sel { AMBER } else { FG });
                ctx.set_font("12px sans-serif");
                ctx.set_text_align(TextAlign::Left);
                ctx.set_text_baseline(TextBaseline::Middle);
                ctx.fill_text(&format!("{} · {}", n.reg, n.city), 20.0, lr.y + lr.h / 2.0);
                ctx.set_fill_color(DIMC);
                ctx.set_font("10px monospace");
                ctx.set_text_align(TextAlign::Right);
                ctx.fill_text(n.ip, w - 20.0, lr.y + lr.h / 2.0);
            }
            MenuRow::Skin(s) => {
                let active = s == skin;
                if active {
                    ctx.set_fill_color("rgba(251,178,106,0.10)");
                    ctx.fill_rect(2.0, lr.y, w - 2.0, lr.h);
                }
                ctx.set_fill_color(if active { AMBER } else { FG });
                ctx.set_font("13px sans-serif");
                ctx.set_text_align(TextAlign::Left);
                ctx.set_text_baseline(TextBaseline::Middle);
                let name = match s {
                    SkinKind::Bloom => "Bloom",
                    SkinKind::Planet => "Planet",
                    SkinKind::Radar => "Radar",
                    SkinKind::Aurora => "Aurora",
                };
                ctx.fill_text(name, 20.0, lr.y + lr.h / 2.0);
                if active {
                    ctx.set_fill_color(AMBER);
                    ctx.begin_path();
                    ctx.arc(w - 22.0, lr.y + lr.h / 2.0, 3.5, 0.0, std::f64::consts::TAU);
                    ctx.fill();
                }
            }
            MenuRow::Action(a) => {
                let label = match a {
                    MenuAction::ImportConfig => "Import config  ·  QR / file",
                    MenuAction::Login => "Log in to zengeld.com",
                };
                ctx.set_fill_color(AMBER);
                ctx.set_font("13px sans-serif");
                ctx.set_text_align(TextAlign::Left);
                ctx.set_text_baseline(TextBaseline::Middle);
                ctx.fill_text(label, 20.0, lr.y + lr.h / 2.0);
                ctx.set_fill_color(DIMC);
                ctx.set_text_align(TextAlign::Right);
                ctx.set_font("14px sans-serif");
                ctx.fill_text("›", w - 18.0, lr.y + lr.h / 2.0);
            }
        }
    }

    // Transient note (e.g. stub feedback for not-yet-built actions).
    if let Some(n) = note {
        ctx.set_fill_color(MUTED);
        ctx.set_font("11px monospace");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(n, w / 2.0, h - 10.0);
    }
}
