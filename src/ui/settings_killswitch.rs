//! Settings → KillSwitch screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::Rect;

use crate::app::{AppState, ConfirmDrawer};
use super::components::{
    BG, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED, ACCENT,
    draw_back_header, draw_divider, draw_toggle,
};

pub fn draw(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    viewport: Rect,
    state: &mut AppState,
) {
    let w = viewport.width;
    let h = viewport.height;

    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, w, h);

    if draw_back_header(ctx, input, state, "KillSwitch", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let content_w = w - margin * 2.0;
    let mut y = 80.0;

    // ----- Main toggle row --------------------------------------------------
    let toggle_row_h = 72.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, toggle_row_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, toggle_row_h, 12.0);

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("16px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Bottom);
    ctx.fill_text("KillSwitch", margin + 16.0, y + toggle_row_h / 2.0 - 1.0);

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(
        "Блокирует все соединения при разрыве VPN",
        margin + 16.0,
        y + toggle_row_h / 2.0 + 3.0,
    );

    let pill_w = 44.0;
    let pill_h = 26.0;
    let pill_x = margin + content_w - pill_w - 16.0;
    let pill_y = y + (toggle_row_h - pill_h) / 2.0;

    // Sync toggle state with dedicated field
    if state.toggle_get("killswitch_enabled") != state.killswitch_enabled {
        state.toggles.insert("killswitch_enabled".into(), state.killswitch_enabled);
    }
    if draw_toggle(ctx, input, state, "killswitch_enabled", pill_x, pill_y) {
        state.killswitch_enabled = state.toggle_get("killswitch_enabled");
    }

    y += toggle_row_h + 16.0;

    // ----- Mode section label -----------------------------------------------
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("12px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("РЕЖИМ", margin + 4.0, y + 8.0);
    y += 24.0;

    // ----- Radio: Мягкий (Soft) mode ----------------------------------------
    let mode_row_h = 60.0;
    let mode_container_h = mode_row_h * 2.0 + 1.0; // 1px separator
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, mode_container_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, mode_container_h, 12.0);

    // Soft mode row
    let soft_is_selected = !state.killswitch_strict;
    draw_mode_row(
        ctx, input, state,
        "ks_mode_soft",
        "Мягкий (Soft)",
        "Блокировка при неожиданном разрыве",
        soft_is_selected,
        margin, y, content_w, mode_row_h,
    );

    draw_divider(ctx, margin + 16.0, y + mode_row_h, content_w - 16.0);

    // Strict mode row
    let strict_is_selected = state.killswitch_strict;
    let strict_clicked = draw_mode_row(
        ctx, input, state,
        "ks_mode_strict",
        "Строгий (Strict)",
        "Блокировка даже при ручном отключении",
        strict_is_selected,
        margin, y + mode_row_h, content_w, mode_row_h,
    );

    if strict_clicked && !state.killswitch_strict {
        state.show_confirm(ConfirmDrawer {
            title: "Включить строгий KillSwitch?".into(),
            description: "Весь интернет будет заблокирован при выключенном VPN.".into(),
            confirm_label: "Включить".into(),
            cancel_label: "Отмена".into(),
            action_id: "ks_strict_enable".into(),
        });
    } else if strict_clicked && state.killswitch_strict {
        // Toggling off — just disable strict
        state.killswitch_strict = false;
        state.toggles.insert("killswitch_strict".into(), false);
    }

    // Soft mode click just sets strict=false (no confirm needed)
    let soft_id = uzor::types::WidgetId::new("ks_mode_soft");
    let soft_hovered = state.coordinator.is_hovered(&soft_id);
    if input.is_clicked() && soft_hovered && state.killswitch_strict {
        state.killswitch_strict = false;
        state.toggles.insert("killswitch_strict".into(), false);
    }

    y += mode_container_h + 16.0;

    // ----- Info text --------------------------------------------------------
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(
        "Строгий KillSwitch заблокирует весь интернет",
        margin + 4.0,
        y,
    );
    ctx.fill_text("при выключенном VPN.", margin + 4.0, y + 18.0);

    let _ = h;
}

/// Draw a radio-button-style mode selection row.
/// Returns `true` if the row was clicked.
fn draw_mode_row(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    id: &str,
    title: &str,
    subtitle: &str,
    is_selected: bool,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> bool {
    use uzor::input::Sense;
    use uzor::types::WidgetId;
    use std::f64::consts::PI;

    let rect = Rect::new(x, y, w, h);
    let widget_id = WidgetId::new(id);
    state.coordinator.register(widget_id.clone(), rect, Sense::CLICK);
    let is_hovered = state.coordinator.is_hovered(&widget_id);
    let clicked = input.is_clicked();

    if is_hovered {
        ctx.set_fill_color_alpha(SURFACE_2, 0.4);
        ctx.fill_rect(x, y, w, h);
    }

    // Radio circle (right side)
    let radio_r = 10.0;
    let radio_cx = x + w - radio_r - 16.0;
    let radio_cy = y + h / 2.0;

    if is_selected {
        // Filled accent circle
        ctx.set_fill_color(ACCENT);
        ctx.begin_path();
        ctx.arc(radio_cx, radio_cy, radio_r, 0.0, 2.0 * PI);
        ctx.fill();
        // Inner white dot
        ctx.set_fill_color("#FFFFFF");
        ctx.begin_path();
        ctx.arc(radio_cx, radio_cy, 4.0, 0.0, 2.0 * PI);
        ctx.fill();
    } else {
        // Outline-only circle
        ctx.set_stroke_color(SURFACE_2);
        ctx.set_stroke_width(1.5);
        ctx.begin_path();
        ctx.arc(radio_cx, radio_cy, radio_r, 0.0, 2.0 * PI);
        ctx.stroke();
    }

    // Text column
    let text_x = x + 16.0;
    let mid_y = y + h / 2.0;

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("15px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Bottom);
    ctx.fill_text(title, text_x, mid_y - 1.0);

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("12px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(subtitle, text_x, mid_y + 3.0);

    clicked && is_hovered
}
