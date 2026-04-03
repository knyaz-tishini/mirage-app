//! Shared color constants, SVG icons, and drawing helpers for Mirage VPN UI.
//! Follows AmneziaVPN's exact dark palette.

use std::f64::consts::PI;

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
pub const SUCCESS: &str = "#4CAF50";       // green for connected/success logs

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

// =============================================================================
// Interactive components (Phase 2)
// =============================================================================

/// Draw an interactive toggle pill (44×26 px).
///
/// ON: orange (`ACCENT`) background. OFF: dark (`SURFACE_2`) background.
/// Reads and writes `state.toggles[id]`. Returns `true` if the value changed this frame.
pub fn draw_toggle(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    id: &str,
    x: f64,
    y: f64,
) -> bool {
    let pill_w = 44.0;
    let pill_h = 26.0;
    let radius = pill_h / 2.0;
    let knob_r = 10.0; // 20px diameter knob
    let padding = 3.0;

    let click_rect = Rect::new(x - 4.0, y - 4.0, pill_w + 8.0, pill_h + 8.0);
    let widget_id = WidgetId::new(format!("toggle_{id}"));
    state.coordinator.register(widget_id.clone(), click_rect, Sense::CLICK);
    let is_hovered = state.coordinator.is_hovered(&widget_id);
    let clicked = input.is_clicked();

    let is_on = state.toggle_get(id);
    let mut changed = false;

    if clicked && is_hovered {
        state.toggle_flip(id);
        changed = true;
    }

    // Re-read after possible flip
    let is_on = if changed { !is_on } else { is_on };

    // Pill background
    let bg_color = if is_on { ACCENT } else { SURFACE_2 };
    ctx.set_fill_color(bg_color);
    ctx.fill_rounded_rect(x, y, pill_w, pill_h, radius);

    // Subtle hover highlight
    if is_hovered {
        ctx.set_fill_color_alpha(TEXT_PRIMARY, 0.08);
        ctx.fill_rounded_rect(x, y, pill_w, pill_h, radius);
    }

    // Knob (white circle)
    let knob_cx = if is_on {
        x + pill_w - padding - knob_r
    } else {
        x + padding + knob_r
    };
    let knob_cy = y + pill_h / 2.0;

    ctx.set_fill_color("#FFFFFF");
    ctx.begin_path();
    ctx.arc(knob_cx, knob_cy, knob_r, 0.0, 2.0 * PI);
    ctx.fill();

    changed
}

/// Draw an editable text input field.
///
/// Clicking the field calls `state.focus_field(field_id, initial_value)`.
/// When focused, draws an accent border and a blinking cursor after the text.
/// Returns `true` if the field gained focus this frame.
pub fn draw_text_field(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    field_id: &str,
    placeholder: &str,
    initial_value: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> bool {
    let rect = Rect::new(x, y, w, h);
    let widget_id = WidgetId::new(format!("textfield_{field_id}"));
    state.coordinator.register(widget_id.clone(), rect, Sense::CLICK);
    let is_hovered = state.coordinator.is_hovered(&widget_id);
    let clicked = input.is_clicked();
    let is_focused = state.focused_field.as_deref() == Some(field_id);
    let mut gained_focus = false;

    if clicked && is_hovered {
        state.focus_field(field_id, initial_value);
        gained_focus = true;
    } else if clicked && !is_hovered && is_focused {
        state.blur_field();
    }

    // Re-check focus after possible state change
    let is_focused = state.focused_field.as_deref() == Some(field_id);

    // Background
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(x, y, w, h, 10.0);

    // Border
    let border_color = if is_focused { ACCENT } else if is_hovered { TEXT_MUTED } else { SURFACE_2 };
    ctx.set_stroke_color(border_color);
    ctx.set_stroke_width(if is_focused { 1.5 } else { 1.0 });
    ctx.stroke_rounded_rect(x, y, w, h, 10.0);

    let text_x = x + 12.0;
    let text_cy = y + h / 2.0;
    let text = state.text_fields.get(field_id).cloned().unwrap_or_default();

    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);

    if text.is_empty() && !is_focused {
        // Placeholder
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("14px pt root ui");
        ctx.fill_text(placeholder, text_x, text_cy);
    } else {
        // Text value
        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("14px pt root ui");
        ctx.fill_text(&text, text_x, text_cy);
    }

    // Blinking cursor when focused
    if is_focused {
        // Blink: visible when (animation_phase * 2.0) as u32 % 2 == 0
        let blink_visible = (state.animation_phase * 2.0) as u32 % 2 == 0;
        if blink_visible {
            // Approximate cursor X: 8px per character (simple estimation)
            let char_count = text.chars().count();
            let cursor_x = text_x + char_count as f64 * 8.0;
            ctx.set_fill_color(ACCENT);
            ctx.fill_rect(cursor_x, y + 6.0, 1.5, h - 12.0);
        }
    }

    gained_focus
}

/// Draw a modal confirmation drawer overlay (slide-up sheet).
///
/// Called from `draw_frame` AFTER screen content when `state.confirm_drawer.is_some()`.
/// Returns `Some(action_id)` if the confirm button was tapped, `None` otherwise.
/// Caller must call `state.dismiss_confirm()` after receiving `Some`.
pub fn draw_confirm_drawer(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    w: f64,
    h: f64,
) -> Option<String> {
    let drawer = match &state.confirm_drawer {
        Some(d) => d.clone(),
        None => return None,
    };

    let sheet_h = 260.0;
    let sheet_y = h - sheet_h;

    // Dim overlay above the sheet
    ctx.set_fill_color_alpha(BG, 0.7);
    ctx.fill_rect(0.0, 0.0, w, sheet_y);

    // Sheet background
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(0.0, sheet_y, w, sheet_h, 16.0);
    // Fill the bottom corners (rounded_rect leaves them rounded)
    ctx.fill_rect(0.0, sheet_y + 16.0, w, sheet_h - 16.0);

    // Drag handle
    ctx.set_fill_color(SURFACE_2);
    ctx.fill_rounded_rect(w / 2.0 - 20.0, sheet_y + 10.0, 40.0, 4.0, 2.0);

    let margin = 20.0;
    let content_w = w - margin * 2.0;
    let mut row_y = sheet_y + 30.0;

    // Title
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 18px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(&drawer.title, w / 2.0, row_y);
    row_y += 30.0;

    // Description (only if non-empty)
    if !drawer.description.is_empty() {
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px pt root ui");
        ctx.fill_text(&drawer.description, w / 2.0, row_y);
        row_y += 24.0;
    }

    row_y += 8.0;

    let btn_h = 52.0;
    let btn_gap = 8.0;

    // Confirm button
    let confirm_rect = Rect::new(margin, row_y, content_w, btn_h);
    let confirm_id = WidgetId::new("confirm_drawer_ok");
    state.coordinator.register(confirm_id.clone(), confirm_rect, Sense::CLICK);
    let confirm_hovered = state.coordinator.is_hovered(&confirm_id);

    let confirm_bg = if confirm_hovered {
        ctx.set_fill_color_alpha(ACCENT, 0.85);
        ACCENT
    } else {
        ACCENT
    };
    ctx.set_fill_color(confirm_bg);
    ctx.fill_rounded_rect(margin, row_y, content_w, btn_h, 12.0);
    ctx.set_fill_color(BG);
    ctx.set_font("bold 15px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(&drawer.confirm_label, w / 2.0, row_y + btn_h / 2.0);

    row_y += btn_h + btn_gap;

    // Cancel button
    let cancel_rect = Rect::new(margin, row_y, content_w, btn_h);
    let cancel_id = WidgetId::new("confirm_drawer_cancel");
    state.coordinator.register(cancel_id.clone(), cancel_rect, Sense::CLICK);
    let cancel_hovered = state.coordinator.is_hovered(&cancel_id);

    let cancel_bg = if cancel_hovered { SURFACE_2 } else { SURFACE_2 };
    ctx.set_fill_color(cancel_bg);
    ctx.fill_rounded_rect(margin, row_y, content_w, btn_h, 12.0);
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("15px pt root ui");
    ctx.fill_text(&drawer.cancel_label, w / 2.0, row_y + btn_h / 2.0);

    // Dim area click = cancel
    let dim_rect = Rect::new(0.0, 0.0, w, sheet_y);
    let dim_id = WidgetId::new("confirm_drawer_dim");
    state.coordinator.register(dim_id.clone(), dim_rect, Sense::CLICK);
    let dim_hovered = state.coordinator.is_hovered(&dim_id);

    let clicked = input.is_clicked();

    if clicked {
        if confirm_hovered {
            return Some(drawer.action_id.clone());
        }
        if cancel_hovered || dim_hovered {
            state.dismiss_confirm();
        }
    }

    // Keep drawer alive — caller uses return value to know if confirmed
    None
}

