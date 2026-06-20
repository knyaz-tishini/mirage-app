//! Top overlay menu with skin switcher.
//!
//! Faithful to the JS reference `mirage-desktop-client.html`: the menu drops
//! from the top (immediately below the 32 px chrome strip), spans the full
//! width, has a dark translucent fill with a 2 px amber left accent, a mono
//! section title, labelled rows, and a "tap M to close" footer.
//!
//! `M` (chrome menu button) opens / closes it. The scrim below catches clicks
//! to dismiss. Hit-zones for the scrim, panel, and skin rows are registered
//! separately in `app.rs` — keep the geometry constants here in sync.

use uzor::render::{RenderContext, TextAlign, TextBaseline};

use crate::centerpiece::SkinKind;

/// Y where the overlay begins — immediately below the chrome strip.
pub const MENU_TOP: f64 = 32.0;
/// Height of the title block (mono section header) above the rows.
pub const MENU_TITLE_H: f64 = 46.0;
/// Height of one skin row.
pub const MENU_ROW_H: f64 = 44.0;
/// Number of skin rows.
pub const MENU_ROW_COUNT: usize = 4;
/// Bottom padding below the last row.
pub const MENU_FOOT_H: f64 = 16.0;

/// Y where the first skin row starts.
pub const fn menu_rows_start() -> f64 {
    MENU_TOP + MENU_TITLE_H
}

/// Total overlay height (title + rows + footer).
pub const fn menu_height() -> f64 {
    MENU_TITLE_H + MENU_ROW_H * MENU_ROW_COUNT as f64 + MENU_FOOT_H
}

// ── palette (mirrors the reference :root tokens) ────────────────────────────────
const BG: &str = "rgba(14,14,17,0.97)";
const SCRIM: &str = "rgba(0,0,0,0.45)";
const AMBER: &str = "#FBB26A";
const LINE: &str = "rgba(255,255,255,0.08)";
const LINE2: &str = "rgba(255,255,255,0.14)";
const FG: &str = "#F4F4F5";

/// Draw the scrim + top overlay with skin-switcher rows.
///
/// Call AFTER the centerpiece is painted (so the overlay sits on top) and
/// BEFORE the chrome strip (so the chrome stays clickable above it).
pub fn draw_menu_overlay(ctx: &mut dyn RenderContext, w: f64, h: f64, active: SkinKind) {
    // ── Scrim (below the chrome strip) ──────────────────────────────────────────
    ctx.set_fill_color(SCRIM);
    ctx.fill_rect(0.0, MENU_TOP, w, (h - MENU_TOP).max(0.0));

    let menu_h = menu_height();

    // ── Panel background ────────────────────────────────────────────────────────
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, MENU_TOP, w, menu_h);

    // 2 px amber left accent.
    ctx.set_fill_color(AMBER);
    ctx.fill_rect(0.0, MENU_TOP, 2.0, menu_h);

    // Bottom hairline.
    ctx.set_fill_color(LINE2);
    ctx.fill_rect(0.0, MENU_TOP + menu_h - 1.0, w, 1.0);

    // ── Section title ───────────────────────────────────────────────────────────
    ctx.set_fill_color(AMBER);
    ctx.set_font("11px monospace");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("SKIN", 22.0, MENU_TOP + 24.0);

    // ── Skin rows ───────────────────────────────────────────────────────────────
    let rows_start = menu_rows_start();
    draw_skin_row(ctx, w, rows_start,                    "Bloom",  active == SkinKind::Bloom);
    draw_skin_row(ctx, w, rows_start + MENU_ROW_H,       "Planet", active == SkinKind::Planet);
    draw_skin_row(ctx, w, rows_start + MENU_ROW_H * 2.0, "Radar",  active == SkinKind::Radar);
    draw_skin_row(ctx, w, rows_start + MENU_ROW_H * 3.0, "Aurora", active == SkinKind::Aurora);
}

/// Draw a single labelled skin row.
fn draw_skin_row(ctx: &mut dyn RenderContext, w: f64, y: f64, label: &str, is_active: bool) {
    // Active highlight fill.
    if is_active {
        ctx.set_fill_color("rgba(251,178,106,0.10)");
        ctx.fill_rect(2.0, y, w - 2.0, MENU_ROW_H);
    }

    // Row bottom divider.
    ctx.set_fill_color(LINE);
    ctx.fill_rect(22.0, y + MENU_ROW_H - 1.0, w - 44.0, 1.0);

    // Label.
    ctx.set_fill_color(if is_active { AMBER } else { FG });
    ctx.set_font("14px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(label, 22.0, y + MENU_ROW_H / 2.0);

    // Active indicator dot on the right.
    if is_active {
        ctx.set_fill_color(AMBER);
        ctx.begin_path();
        ctx.arc(w - 24.0, y + MENU_ROW_H / 2.0, 4.0, 0.0, std::f64::consts::TAU);
        ctx.fill();
    }
}
