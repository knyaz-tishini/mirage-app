//! Bottom-sheet menu overlay with skin switcher.
//!
//! The sheet slides up from the bottom of the window when `menu_open` is true.
//! It contains a "Skin" section header and three labelled rows (Bloom / Planet
//! / Radar). The active skin row is highlighted with an accent fill.

use uzor::render::{RenderContext, TextAlign, TextBaseline};

use crate::centerpiece::SkinKind;

/// Height of the bottom sheet in logical pixels (≈60% of the 540 px window).
pub const SHEET_HEIGHT: f64 = 324.0;

/// Draw the scrim + bottom sheet with skin-switcher rows.
///
/// Call this AFTER the titlebar has been painted (so the sheet renders on top).
/// Hit-zones for the scrim and skin rows are registered separately in `app.rs`.
pub fn draw_menu_sheet(ctx: &mut dyn RenderContext, w: f64, h: f64, active: SkinKind) {
    let sheet_y = h - SHEET_HEIGHT;

    // ── Scrim ─────────────────────────────────────────────────────────────────
    ctx.set_fill_color("rgba(0,0,0,0.55)");
    ctx.fill_rect(0.0, 0.0, w, h);

    // ── Sheet background ──────────────────────────────────────────────────────
    ctx.set_fill_color("#111116");
    ctx.fill_rect(0.0, sheet_y, w, SHEET_HEIGHT);

    // Top border.
    ctx.set_stroke_color("rgba(255,255,255,0.08)");
    ctx.set_stroke_width(1.0);
    ctx.begin_path();
    ctx.move_to(0.0, sheet_y);
    ctx.line_to(w, sheet_y);
    ctx.stroke();

    // ── Section header ────────────────────────────────────────────────────────
    ctx.set_fill_color("#6B6F7A");
    ctx.set_font("11px DM Sans");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    // Vertically centre in the 40 px header zone.
    ctx.fill_text("Skin", 20.0, sheet_y + 20.0);

    // ── Skin rows: Bloom / Planet / Radar ─────────────────────────────────────
    // Matches the hit-zone layout in app.rs:
    //   rows_start = sheet_y + 40, row_h = 48
    let rows_start = sheet_y + 40.0;
    let row_h = 48.0;

    draw_skin_row(ctx, w, rows_start,           "Bloom",  active == SkinKind::Bloom);
    draw_skin_row(ctx, w, rows_start + row_h,   "Planet", active == SkinKind::Planet);
    draw_skin_row(ctx, w, rows_start + row_h * 2.0, "Radar",  active == SkinKind::Radar);
}

/// Draw a single labelled skin row.
fn draw_skin_row(ctx: &mut dyn RenderContext, w: f64, y: f64, label: &str, is_active: bool) {
    // Active highlight fill.
    if is_active {
        ctx.set_fill_color("rgba(251,178,106,0.12)");
        ctx.fill_rect(0.0, y, w, 48.0);
    }

    // Row bottom divider.
    ctx.set_stroke_color("rgba(255,255,255,0.05)");
    ctx.set_stroke_width(1.0);
    ctx.begin_path();
    ctx.move_to(20.0, y + 48.0);
    ctx.line_to(w - 20.0, y + 48.0);
    ctx.stroke();

    // Label.
    let text_color = if is_active { "#FBB26A" } else { "#D4D4D8" };
    ctx.set_fill_color(text_color);
    ctx.set_font("14px DM Sans");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(label, 20.0, y + 24.0);

    // Active indicator dot on the right.
    if is_active {
        ctx.set_fill_color("#FBB26A");
        ctx.begin_path();
        ctx.arc(w - 20.0, y + 24.0, 4.0, 0.0, std::f64::consts::TAU);
        ctx.fill();
    }
}
