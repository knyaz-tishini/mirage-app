//! Shared color constants, SVG icons, and drawing helpers for Mirage VPN UI.
//! Follows AmneziaVPN's exact dark palette.

use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::input::{InputState, Sense};
use uzor::types::{Rect, WidgetId};

use crate::app::AppState;

// =============================================================================
// Color palette — AmneziaVPN dark theme
// =============================================================================

pub const BG: &str = "#0E0E11";           // midnightBlack
pub const SURFACE: &str = "#1C1D21";       // onyxBlack
pub const SURFACE_2: &str = "#2C2D30";     // slateGray (borders/dividers)
pub const ACCENT: &str = "#FBB26A";        // goldenApricot (PRIMARY orange)
pub const ERROR_COLOR: &str = "#EB5757";   // vibrantRed
pub const TEXT_PRIMARY: &str = "#D7D8DB";  // paleGray
pub const TEXT_MUTED: &str = "#878B91";    // mutedGray
pub const SEPARATOR: &str = "#2C2D30";     // slateGray
pub const NAV_BG: &str = "#1C1D21";       // onyxBlack

// =============================================================================
// SVG icon constants — from uzor UI icon library (Feather/Lucide style)
// =============================================================================

pub use uzor::render::icons::ui::ICON_HOME;
pub use uzor::render::icons::ui::ICON_SERVER;
pub use uzor::render::icons::ui::ICON_SETTINGS;
pub use uzor::render::icons::ui::ICON_FILE_TEXT as ICON_LOGS;
pub use uzor::render::icons::ui::ICON_CHEVRON_RIGHT;
pub use uzor::render::icons::ui::ICON_CHEVRON_DOWN;
pub use uzor::render::icons::ui::ICON_CHEVRON_LEFT as ICON_ARROW_LEFT;
pub use uzor::render::icons::ui::ICON_PLUS;

// =============================================================================
// Nav bar constants
// =============================================================================

pub const NAV_HEIGHT: f64 = 52.0;

/// Draw the bottom navigation bar — centered icon group, no labels.
///
/// Returns `Some(index)` if a tab was clicked this frame, `None` otherwise.
pub fn draw_nav_bar(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    width: f64,
    height: f64,
    active_tab: usize,
) -> Option<usize> {
    let nav_y = height - NAV_HEIGHT;
    let tab_count = 4usize;
    let icon_size = 32.0;
    let icon_gap = 24.0;
    let btn_w = icon_size + icon_gap;
    let group_w = btn_w * tab_count as f64;
    let group_x = (width - group_w) / 2.0;
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

    let icons = [ICON_HOME, ICON_SERVER, ICON_SETTINGS, ICON_PLUS];

    let mut result = None;

    for i in 0..tab_count {
        let bx = group_x + i as f64 * btn_w;
        let nav_rect = Rect::new(bx, nav_y, btn_w, NAV_HEIGHT);
        let nav_id = WidgetId::new(format!("nav_{i}"));

        state.coordinator.register(nav_id.clone(), nav_rect, Sense::CLICK);
        let is_hovered = state.coordinator.is_hovered(&nav_id);
        let is_active = active_tab == i;
        let icon_color = if is_active { ACCENT } else if is_hovered { TEXT_PRIMARY } else { TEXT_MUTED };

        let icon_x = bx + (btn_w - icon_size) / 2.0;
        let icon_y = nav_y + (NAV_HEIGHT - icon_size) / 2.0;

        draw_svg_icon(ctx, icons[i], icon_x, icon_y, icon_size, icon_size, icon_color);

        if clicked && is_hovered {
            result = Some(i);
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

/// Draw a back header with left arrow + title.
///
/// Returns `true` if the back button was clicked.
pub fn draw_back_header(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    title: &str,
    _w: f64,
) -> bool {
    let clicked = input.is_clicked();
    let btn_size = 40.0;
    let margin = 16.0;
    let header_y = 20.0;

    // Back arrow button area
    let btn_rect = Rect::new(margin, header_y, btn_size, btn_size);
    let btn_id = WidgetId::new("back_btn");
    state.coordinator.register(btn_id.clone(), btn_rect, Sense::CLICK);
    let btn_hovered = state.coordinator.is_hovered(&btn_id);

    if btn_hovered {
        ctx.set_fill_color_alpha(SURFACE_2, 0.5);
        ctx.fill_rounded_rect(margin, header_y, btn_size, btn_size, 8.0);
    }

    let icon_size = 24.0;
    let icon_x = margin + (btn_size - icon_size) / 2.0;
    let icon_y = header_y + (btn_size - icon_size) / 2.0;
    draw_svg_icon(ctx, ICON_ARROW_LEFT, icon_x, icon_y, icon_size, icon_size, TEXT_PRIMARY);

    // Title text
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 20px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(title, margin + btn_size + 12.0, header_y + btn_size / 2.0);

    clicked && btn_hovered
}

/// Draw a horizontal tab bar. Returns `Some(index)` if a tab was clicked.
pub fn draw_tab_bar(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    tabs: &[&str],
    active: usize,
    x: f64,
    y: f64,
    total_w: f64,
) -> Option<usize> {
    let clicked = input.is_clicked();
    let tab_w = total_w / tabs.len() as f64;
    let tab_h = 40.0;
    let mut result = None;

    // Background
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(x, y, total_w, tab_h, 10.0);

    for (i, &tab) in tabs.iter().enumerate() {
        let tx = x + i as f64 * tab_w;
        let tab_rect = Rect::new(tx, y, tab_w, tab_h);
        let tab_id = WidgetId::new(format!("tab_bar_{i}"));
        state.coordinator.register(tab_id.clone(), tab_rect, Sense::CLICK);
        let is_hovered = state.coordinator.is_hovered(&tab_id);
        let is_active = active == i;

        if is_active {
            ctx.set_fill_color(SURFACE_2);
            ctx.fill_rounded_rect(tx + 2.0, y + 2.0, tab_w - 4.0, tab_h - 4.0, 8.0);
        } else if is_hovered {
            ctx.set_fill_color_alpha(SURFACE_2, 0.4);
            ctx.fill_rounded_rect(tx + 2.0, y + 2.0, tab_w - 4.0, tab_h - 4.0, 8.0);
        }

        let text_color = if is_active { TEXT_PRIMARY } else { TEXT_MUTED };
        ctx.set_fill_color(text_color);
        ctx.set_font("13px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(tab, tx + tab_w / 2.0, y + tab_h / 2.0);

        // Accent underline for active tab
        if is_active {
            let ul_w = tab_w * 0.5;
            let ul_x = tx + (tab_w - ul_w) / 2.0;
            ctx.set_fill_color(ACCENT);
            ctx.fill_rounded_rect(ul_x, y + tab_h - 3.0, ul_w, 2.0, 1.0);
        }

        if clicked && is_hovered {
            result = Some(i);
        }
    }

    result
}

/// Draw a standard Amnezia-style list row: [icon] [title + subtitle] [chevron].
///
/// Returns `true` if the row was clicked.
pub fn draw_list_item(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    id: &str,
    icon_svg: &str,
    title: &str,
    subtitle: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> bool {
    let rect = Rect::new(x, y, w, h);
    let widget_id = WidgetId::new(id);
    state.coordinator.register(widget_id.clone(), rect, Sense::CLICK);
    let is_hovered = state.coordinator.is_hovered(&widget_id);
    let clicked = input.is_clicked();

    if is_hovered {
        ctx.set_fill_color_alpha(SURFACE_2, 0.5);
        ctx.fill_rect(x, y, w, h);
    }

    let icon_size = 24.0;
    let icon_margin = 16.0;
    let icon_y_center = y + h / 2.0;

    // Left icon
    draw_svg_icon(
        ctx,
        icon_svg,
        x + icon_margin,
        icon_y_center - icon_size / 2.0,
        icon_size,
        icon_size,
        TEXT_MUTED,
    );

    // Text column
    let text_x = x + icon_margin + icon_size + 14.0;

    if subtitle.is_empty() {
        // Single title centered
        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("16px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(title, text_x, icon_y_center);
    } else {
        // Title + subtitle stacked
        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("16px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(title, text_x, icon_y_center - 1.0);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(subtitle, text_x, icon_y_center + 3.0);
    }

    // Chevron right
    let chevron_size = 20.0;
    draw_svg_icon(
        ctx,
        ICON_CHEVRON_RIGHT,
        x + w - chevron_size - 14.0,
        icon_y_center - chevron_size / 2.0,
        chevron_size,
        chevron_size,
        TEXT_MUTED,
    );

    clicked && is_hovered
}

/// Draw a settings list item row: [SVG icon 24x24] [Title] [chevron-right SVG].
///
/// Returns true if this row was clicked.
pub fn draw_settings_row(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    row_id: &str,
    icon_svg: &str,
    title: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> bool {
    draw_list_item(ctx, input, state, row_id, icon_svg, title, "", x, y, w, h)
}
