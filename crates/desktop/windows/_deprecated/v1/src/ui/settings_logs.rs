//! Settings → Логи screen — log viewer with scroll support.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::{Rect, WidgetId};
use uzor::input::Sense;

use crate::app::{AppState, ConfirmDrawer};
use super::components::{
    BG, SURFACE, SURFACE_2, TEXT_MUTED, ACCENT, ERROR_COLOR, SUCCESS, NAV_HEIGHT,
    draw_back_header, draw_divider,
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

    // Header row with back button
    if draw_back_header(ctx, input, state, "Логи", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let header_y = 20.0;
    let btn_size = 40.0;
    let clicked = input.is_clicked();

    // "Очистить" button (top-right)
    let clear_btn_w = 88.0;
    let clear_btn_x = w - margin - clear_btn_w;
    let clear_id = WidgetId::new("logs_clear_btn");
    let clear_rect = Rect::new(clear_btn_x, header_y, clear_btn_w, btn_size);
    state.coordinator.register(clear_id.clone(), clear_rect, Sense::CLICK);
    let clear_hovered = state.coordinator.is_hovered(&clear_id);

    if clear_hovered {
        ctx.set_fill_color_alpha(SURFACE_2, 0.7);
        ctx.fill_rounded_rect(clear_btn_x, header_y, clear_btn_w, btn_size, 8.0);
    }
    ctx.set_fill_color(ERROR_COLOR);
    ctx.set_font("13px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Очистить", clear_btn_x + clear_btn_w / 2.0, header_y + btn_size / 2.0);

    if clicked && clear_hovered {
        state.show_confirm(ConfirmDrawer {
            title: "Очистить логи?".into(),
            description: "Все записи будут удалены.".into(),
            confirm_label: "Очистить".into(),
            cancel_label: "Отмена".into(),
            action_id: "clear_logs".into(),
        });
    }

    let content_w = w - margin * 2.0;
    let log_top = 76.0;
    let log_bottom = h - NAV_HEIGHT - 16.0;
    let log_h = log_bottom - log_top;

    // Log container background
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, log_top, content_w, log_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, log_top, content_w, log_h, 12.0);

    let line_h = 18.0;
    let pad = 12.0;
    let visible_h = log_h - pad * 2.0;

    // Augment log_lines with fake timestamped entries if empty
    let effective_lines: Vec<String> = if state.log_lines.is_empty() {
        Vec::new()
    } else {
        state.log_lines.clone()
    };

    let content_h = effective_lines.len() as f64 * line_h;
    let max_scroll = (content_h - visible_h).max(0.0);

    // Scroll wheel
    let scroll_dy = input.scroll_delta.1;
    if scroll_dy.abs() > 0.5 {
        state.scroll_by("logs", scroll_dy, max_scroll);
    }

    // Auto-scroll to bottom by default
    let offset = *state.scroll_offsets.entry("logs".to_string()).or_insert(max_scroll);

    // Clip to log area
    ctx.save();
    ctx.clip_rect(margin + 1.0, log_top + 1.0, content_w - 2.0, log_h - 2.0);
    ctx.translate(0.0, log_top + pad - offset);

    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);

    for (i, line) in effective_lines.iter().enumerate() {
        let line_y = i as f64 * line_h;

        let color = if line.contains("Ошибка") || line.contains("Error") {
            ERROR_COLOR
        } else if line.contains("Подключено") || line.contains("Connected") {
            SUCCESS
        } else if line.contains("DNS") || line.contains("KillSwitch") {
            ACCENT
        } else {
            TEXT_MUTED
        };

        ctx.set_fill_color(color);
        ctx.fill_text(line, margin + pad, line_y);
    }

    ctx.restore();

    if effective_lines.is_empty() {
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("14px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("Логи пусты", margin + content_w / 2.0, log_top + log_h / 2.0);
    }

    // Scrollbar track (4px wide, right edge)
    if max_scroll > 0.0 {
        let track_x = margin + content_w - 6.0;
        let track_y = log_top + pad;
        let track_h = visible_h;

        ctx.set_fill_color(SURFACE_2);
        ctx.fill_rounded_rect(track_x, track_y, 4.0, track_h, 2.0);

        let thumb_ratio = (visible_h / content_h).min(1.0);
        let thumb_h = (track_h * thumb_ratio).max(20.0);
        let thumb_y = track_y + (offset / max_scroll) * (track_h - thumb_h);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.fill_rounded_rect(track_x, thumb_y, 4.0, thumb_h, 2.0);
    }

    // Divider under header
    draw_divider(ctx, margin, log_top - 4.0, content_w);
}
