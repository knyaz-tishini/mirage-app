//! Settings → Логи screen — log viewer.

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

    if draw_back_header(ctx, input, state, "Логи", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let content_w = w - margin * 2.0;
    let log_top = 76.0;
    let log_bottom = h - 16.0;
    let log_h = log_bottom - log_top;

    // Log container
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, log_top, content_w, log_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, log_top, content_w, log_h, 12.0);

    // Clip to log area
    ctx.save();
    ctx.clip_rect(margin, log_top, content_w, log_h);

    let line_h = 18.0;
    let pad = 12.0;
    let visible_lines = ((log_h - pad * 2.0) / line_h) as usize;

    let log_lines = &state.log_lines;
    let start = log_lines.len().saturating_sub(visible_lines);

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);

    for (i, line) in log_lines[start..].iter().enumerate() {
        let line_y = log_top + pad + i as f64 * line_h;
        ctx.fill_text(line, margin + pad, line_y);
    }

    if log_lines.is_empty() {
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("14px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("Логи пусты", margin + content_w / 2.0, log_top + log_h / 2.0);
    }

    ctx.restore();

    // Suppress unused
    let _ = TEXT_PRIMARY;
}
