//! Settings → DNS-серверы screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::Rect;

use crate::app::AppState;
use super::components::{
    BG, SURFACE, SURFACE_2, ACCENT, TEXT_PRIMARY, TEXT_MUTED,
    draw_back_header,
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

    if draw_back_header(ctx, input, state, "DNS-серверы", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let content_w = w - margin * 2.0;
    let field_h = 52.0;
    let mut y = 80.0;

    // Primary DNS
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Основной DNS", margin, y);

    y += 22.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, field_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, field_h, 12.0);

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("16px pt root ui");
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("1.1.1.1", margin + 16.0, y + field_h / 2.0);

    y += field_h + 16.0;

    // Secondary DNS
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Резервный DNS", margin, y);

    y += 22.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, field_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, field_h, 12.0);

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("16px pt root ui");
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("1.0.0.1", margin + 16.0, y + field_h / 2.0);

    y += field_h + 24.0;

    // "Сохранить" button
    let btn_h = 52.0;
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.fill_rounded_rect(margin, y, content_w, btn_h, 16.0);

    ctx.set_fill_color(BG);
    ctx.set_font("bold 16px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Сохранить", margin + content_w / 2.0, y + btn_h / 2.0);

    y += btn_h + 12.0;

    // "Восстановить по умолчанию" ghost button
    ctx.set_fill_color(ACCENT);
    ctx.set_font("14px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Восстановить по умолчанию", margin + content_w / 2.0, y);

    let _ = h;
}
