//! Settings → Split tunneling screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::Rect;

use crate::app::AppState;
use super::components::{
    BG, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED,
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

    if draw_back_header(ctx, input, state, "Split tunneling", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let content_w = w - margin * 2.0;
    let y = 80.0;

    // Toggle
    let toggle_h = 56.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, toggle_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, toggle_h, 12.0);

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("16px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Раздельное туннелирование", margin + 16.0, y + toggle_h / 2.0);

    // Toggle off
    let pill_w = 44.0;
    let pill_h = 26.0;
    let pill_x = margin + content_w - pill_w - 16.0;
    let pill_y = y + (toggle_h - pill_h) / 2.0;
    ctx.set_fill_color(SURFACE_2);
    ctx.fill_rounded_rect(pill_x, pill_y, pill_w, pill_h, 13.0);

    // Description
    let desc_y = y + toggle_h + 20.0;
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(
        "Позволяет подключаться к некоторым сайтам",
        margin,
        desc_y,
    );
    ctx.fill_text(
        "без использования VPN.",
        margin,
        desc_y + 20.0,
    );

    // Empty site list placeholder
    let list_top = desc_y + 56.0;
    let list_h = h - list_top - 80.0;

    if list_h > 0.0 {
        ctx.set_fill_color(SURFACE);
        ctx.fill_rounded_rect(margin, list_top, content_w, list_h, 12.0);
        ctx.set_stroke_color(SURFACE_2);
        ctx.set_stroke_width(1.0);
        ctx.stroke_rounded_rect(margin, list_top, content_w, list_h, 12.0);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("14px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(
            "Список сайтов пуст",
            margin + content_w / 2.0,
            list_top + list_h / 2.0,
        );
    }
}
