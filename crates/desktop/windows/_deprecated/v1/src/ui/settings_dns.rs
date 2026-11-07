//! Settings → DNS-серверы screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::{Rect, WidgetId};
use uzor::input::Sense;

use crate::app::AppState;
use super::components::{
    BG, SURFACE, SURFACE_2, ACCENT, TEXT_PRIMARY, TEXT_MUTED, ERROR_COLOR,
    draw_back_header, draw_text_field, draw_divider,
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

    if draw_back_header(ctx, input, state, "DNS", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let content_w = w - margin * 2.0;
    let field_h = 52.0;
    let mut y = 80.0;

    // ── Section label ──────────────────────────────────────────────────────
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("DNS-СЕРВЕРЫ", margin + 4.0, y);
    y += 20.0;

    // ── Card ───────────────────────────────────────────────────────────────
    let card_h = 16.0 + 18.0 + field_h + 16.0 + 16.0 + 18.0 + field_h + 16.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, card_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, card_h, 16.0);

    let field_x = margin + 16.0;
    let field_w = content_w - 32.0;

    // Primary DNS label
    y += 16.0;
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("12px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Основной DNS", field_x, y);
    y += 18.0;

    draw_text_field(
        ctx, input, state,
        "dns_primary",
        "1.1.1.1",
        &state.dns_primary.clone(),
        field_x, y, field_w, field_h,
    );

    let primary_val = state.field_value("dns_primary").to_string();
    if !primary_val.is_empty() && !is_valid_ip_or_hostname(&primary_val) {
        ctx.set_fill_color(ERROR_COLOR);
        ctx.set_font("11px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("Некорректный IP адрес", field_x, y + field_h + 2.0);
    }

    y += field_h + 16.0;

    draw_divider(ctx, field_x, y, field_w);
    y += 16.0;

    // Secondary DNS label
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("12px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Резервный DNS", field_x, y);
    y += 18.0;

    draw_text_field(
        ctx, input, state,
        "dns_secondary",
        "8.8.8.8",
        &state.dns_secondary.clone(),
        field_x, y, field_w, field_h,
    );

    let secondary_val = state.field_value("dns_secondary").to_string();
    if !secondary_val.is_empty() && !is_valid_ip_or_hostname(&secondary_val) {
        ctx.set_fill_color(ERROR_COLOR);
        ctx.set_font("11px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("Некорректный IP адрес", field_x, y + field_h + 2.0);
    }

    y += field_h + 16.0;

    let clicked = input.is_clicked();

    // ── "Сохранить" button ─────────────────────────────────────────────────
    y += 8.0;
    let btn_h = 52.0;
    let can_save = is_valid_ip_or_hostname(&primary_val)
        && (secondary_val.is_empty() || is_valid_ip_or_hostname(&secondary_val));

    let save_id = WidgetId::new("dns_save_btn");
    let save_rect = Rect::new(margin, y, content_w, btn_h);
    state.coordinator.register(save_id.clone(), save_rect, Sense::CLICK);
    let save_hovered = state.coordinator.is_hovered(&save_id);

    let btn_bg = if can_save { ACCENT } else { SURFACE_2 };
    ctx.set_fill_color(btn_bg);
    ctx.fill_rounded_rect(margin, y, content_w, btn_h, 16.0);

    let btn_text_color = if can_save { BG } else { TEXT_MUTED };
    ctx.set_fill_color(btn_text_color);
    ctx.set_font("bold 15px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Сохранить", w / 2.0, y + btn_h / 2.0);

    if clicked && save_hovered && can_save {
        state.dns_primary = state.field_value("dns_primary").to_string();
        state.dns_secondary = state.field_value("dns_secondary").to_string();
        state.push_log("DNS сохранён");
        state.blur_field();
    }

    y += btn_h + 12.0;

    // ── "Восстановить по умолчанию" ────────────────────────────────────────
    let restore_id = WidgetId::new("dns_restore_btn");
    let restore_rect = Rect::new(margin, y, content_w, 36.0);
    state.coordinator.register(restore_id.clone(), restore_rect, Sense::CLICK);
    let restore_hovered = state.coordinator.is_hovered(&restore_id);

    let link_color = if restore_hovered { TEXT_PRIMARY } else { ACCENT };
    ctx.set_fill_color(link_color);
    ctx.set_font("14px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Восстановить по умолчанию", w / 2.0, y + 18.0);

    if clicked && restore_hovered {
        state.field_set_text("dns_primary", "1.1.1.1".into());
        state.field_set_text("dns_secondary", "8.8.8.8".into());
        state.dns_primary = "1.1.1.1".into();
        state.dns_secondary = "8.8.8.8".into();
    }

    let _ = h;
    let _ = TEXT_PRIMARY;
}

/// Simple IP address or hostname validator (no regex, no dependencies).
fn is_valid_ip_or_hostname(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok()) {
        return true;
    }
    !s.is_empty() && !s.contains(' ')
}
