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
// SVG icon constants — re-exported from uzor built-in icon library
// =============================================================================

pub use uzor::macos::icons::paths::HOME as ICON_HOME;
pub use uzor::macos::icons::paths::SERVER as ICON_SERVER;
pub use uzor::macos::icons::paths::GEAR as ICON_SETTINGS;
pub use uzor::macos::icons::paths::DOCUMENT as ICON_LOGS;
pub use uzor::macos::icons::paths::CHECKMARK as ICON_CHECK;
pub use uzor::macos::icons::paths::CHEVRON_RIGHT as ICON_CHEVRON_RIGHT;

// =============================================================================
// Nav bar constants
// =============================================================================

pub const NAV_HEIGHT: f64 = 72.0;

const NAV_LABELS: [&str; 4] = ["Home", "Servers", "Settings", "Logs"];

/// Draw the bottom navigation bar with SVG icons and text labels.
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
        let label_color = if is_active { ACCENT } else { TEXT_MUTED };
        let icon_size = 22.0;
        let icon_x = bx + (btn_w - icon_size) / 2.0;
        let icon_y = nav_y + 10.0;

        // Active indicator bar at top
        if is_active {
            ctx.set_fill_color(ACCENT);
            ctx.fill_rect(bx + btn_w / 2.0 - 14.0, nav_y, 28.0, 2.5);
        }

        // SVG icon
        draw_svg_icon(ctx, icons[i], icon_x, icon_y, icon_size, icon_size, icon_color);

        // Label text below icon
        ctx.set_fill_color(label_color);
        ctx.set_font("10px sans-serif");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(NAV_LABELS[i], bx + btn_w / 2.0, icon_y + icon_size + 4.0);

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
