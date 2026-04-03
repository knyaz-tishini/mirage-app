//! Shared color constants, SVG icons, and drawing helpers for Mirage VPN UI.
//! Follows AmneziaVPN's exact dark palette.

use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::input::InputState;
use uzor::types::Rect;

// =============================================================================
// Color palette — AmneziaVPN dark theme
// =============================================================================

pub const BG: &str = "#0E0E11";           // midnightBlack
pub const SURFACE: &str = "#1C1D21";       // onyxBlack
pub const SURFACE_2: &str = "#2C2D30";     // slateGray (borders/dividers)
pub const ACCENT: &str = "#FBB26A";        // goldenApricot (PRIMARY orange)
pub const SUCCESS: &str = "#4caf50";       // keep green for connected
pub const WARNING: &str = "#FBB26A";       // use accent for connecting
pub const ERROR_COLOR: &str = "#EB5757";   // vibrantRed
pub const TEXT_PRIMARY: &str = "#D7D8DB";  // paleGray
pub const TEXT_MUTED: &str = "#878B91";    // mutedGray
pub const SEPARATOR: &str = "#2C2D30";     // slateGray
pub const NAV_BG: &str = "#1C1D21";       // onyxBlack

// =============================================================================
// SVG icon constants — custom icons uzor doesn't provide
// =============================================================================

pub const ICON_HOME: &str = r#"<svg viewBox="0 0 24 24"><path d="M3 12l9-9 9 9M5 10v10a1 1 0 001 1h3a1 1 0 001-1v-4a1 1 0 011-1h2a1 1 0 011 1v4a1 1 0 001 1h3a1 1 0 001-1V10" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

pub const ICON_SETTINGS: &str = r#"<svg viewBox="0 0 24 24"><path d="M12 15a3 3 0 100-6 3 3 0 000 6z" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 010 2.83 2 2 0 01-2.83 0l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09A1.65 1.65 0 009 19.4a1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06A1.65 1.65 0 004.68 15a1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09A1.65 1.65 0 004.6 9a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06A1.65 1.65 0 009 4.68a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06A1.65 1.65 0 0019.4 9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z" fill="none" stroke="currentColor" stroke-width="1.5"/></svg>"#;

pub const ICON_SERVER: &str = r#"<svg viewBox="0 0 24 24"><rect x="2" y="2" width="20" height="8" rx="2" ry="2" fill="none" stroke="currentColor" stroke-width="1.5"/><rect x="2" y="14" width="20" height="8" rx="2" ry="2" fill="none" stroke="currentColor" stroke-width="1.5"/><line x1="6" y1="6" x2="6" y2="6" stroke="currentColor" stroke-width="2" stroke-linecap="round"/><line x1="6" y1="18" x2="6" y2="18" stroke="currentColor" stroke-width="2" stroke-linecap="round"/></svg>"#;

pub const ICON_LOGS: &str = r#"<svg viewBox="0 0 24 24"><path d="M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8z" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/><polyline points="14,2 14,8 20,8" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/><line x1="16" y1="13" x2="8" y2="13" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><line x1="16" y1="17" x2="8" y2="17" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><polyline points="10,9 9,9 8,9" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>"#;

// Built-in uzor icons used for checkmark and chevron right
pub use uzor::macos::icons::paths::CHECKMARK as ICON_CHECK;
pub use uzor::macos::icons::paths::CHEVRON_RIGHT as ICON_CHEVRON_RIGHT;

// =============================================================================
// Nav bar constants
// =============================================================================

pub const NAV_HEIGHT: f64 = 72.0;

/// Draw the bottom navigation bar with SVG icons.
///
/// Returns `Some(index)` if a tab was clicked this frame, `None` otherwise.
pub fn draw_nav_bar(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    width: f64,
    height: f64,
    active_tab: usize,
) -> Option<usize> {
    let nav_y = height - NAV_HEIGHT;
    let tab_count = 4usize;
    let btn_w = width / tab_count as f64;
    let clicked = input.is_clicked();

    // Background
    ctx.set_fill_color(NAV_BG);
    ctx.fill_rect(0.0, nav_y, width, NAV_HEIGHT);

    // Top separator line
    ctx.set_stroke_color(SEPARATOR);
    ctx.set_stroke_width(0.5);
    ctx.begin_path();
    ctx.move_to(0.0, nav_y);
    ctx.line_to(width, nav_y);
    ctx.stroke();

    let icons = [ICON_HOME, ICON_SERVER, ICON_SETTINGS, ICON_LOGS];

    let mut result = None;

    for i in 0..tab_count {
        let bx = i as f64 * btn_w;
        let is_active = active_tab == i;
        let icon_color = if is_active { ACCENT } else { TEXT_MUTED };
        let icon_size = 22.0;
        let icon_x = bx + (btn_w - icon_size) / 2.0;
        let icon_y = nav_y + 12.0;

        // Active indicator bar at top
        if is_active {
            ctx.set_fill_color(ACCENT);
            ctx.fill_rect(bx + btn_w / 2.0 - 14.0, nav_y, 28.0, 2.5);
        }

        // SVG icon
        draw_svg_icon(ctx, icons[i], icon_x, icon_y, icon_size, icon_size, icon_color);

        // Hit detection via InputState
        if clicked {
            let nav_rect = Rect::new(bx, nav_y, btn_w, NAV_HEIGHT);
            if input.is_hovered(&nav_rect) {
                result = Some(i);
            }
        }
    }

    result
}

/// Draw a thin horizontal divider line.
pub fn draw_divider(ctx: &mut dyn RenderContext, x: f64, y: f64, w: f64) {
    ctx.set_stroke_color(SEPARATOR);
    ctx.set_stroke_width(0.5);
    ctx.begin_path();
    ctx.move_to(x, y);
    ctx.line_to(x + w, y);
    ctx.stroke();
}

/// Draw a settings list item row: [SVG icon 24x24] [Title] [chevron-right SVG].
///
/// Returns true if this row was clicked.
pub fn draw_settings_row(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    icon_svg: &str,
    title: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> bool {
    let row_rect = Rect::new(x, y, w, h);
    let is_hovered = input.is_hovered(&row_rect);
    let clicked = input.is_clicked();

    // Hover highlight
    if is_hovered {
        ctx.set_fill_color_alpha(SURFACE_2, 0.5);
        ctx.fill_rect(x, y, w, h);
    }

    let icon_size = 22.0;
    let icon_margin = 16.0;
    let icon_y = y + (h - icon_size) / 2.0;

    // Left SVG icon
    draw_svg_icon(ctx, icon_svg, x + icon_margin, icon_y, icon_size, icon_size, TEXT_MUTED);

    // Title text
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("15px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(title, x + icon_margin + icon_size + 12.0, y + h / 2.0);

    // Chevron right — uzor built-in icon
    let chevron_size = 16.0;
    draw_svg_icon(
        ctx,
        ICON_CHEVRON_RIGHT,
        x + w - chevron_size - 14.0,
        y + (h - chevron_size) / 2.0,
        chevron_size,
        chevron_size,
        TEXT_MUTED,
    );

    clicked && is_hovered
}
