//! Logs screen — scrollable log viewer using uzor ScrollState.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::types::Rect;
use uzor::input::InputState;

use crate::app::AppState;
use super::components::{
    BG, SURFACE, SURFACE_2, SUCCESS, ERROR_COLOR, TEXT_PRIMARY, TEXT_MUTED, SEPARATOR, NAV_HEIGHT,
    draw_nav_bar,
};
use super::main_screen::navigate_tab;

pub fn draw_logs_screen(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    viewport: Rect,
    state: &mut AppState,
) {
    let w = viewport.width;
    let h = viewport.height;

    // Background
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, w, h);

    // Title
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 18px sans-serif");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Logs", w / 2.0, 24.0);

    let margin = 16.0;
    let content_top = 58.0;
    let content_bottom = h - NAV_HEIGHT - 8.0;
    let content_w = w - margin * 2.0;
    let content_h = content_bottom - content_top;

    // Line metrics
    let line_h = 20.0;
    let lines_count = state.log_lines.len();
    let total_content_h = lines_count as f64 * line_h + 16.0;

    // Auto-scroll to bottom when new log lines are added.
    if lines_count != state.prev_log_count {
        state.prev_log_count = lines_count;
        let max_scroll = (total_content_h - content_h).max(0.0);
        state.logs_scroll.offset = max_scroll;
    }

    // Scroll via uzor ScrollState — handle_wheel applies physics clamping
    let scroll_delta = input.scroll_delta.1;
    if scroll_delta != 0.0 {
        let scroll_area_rect = Rect::new(margin, content_top, content_w, content_h);
        if input.is_hovered(&scroll_area_rect) {
            state.logs_scroll.handle_wheel(scroll_delta, total_content_h, content_h);
        }
    }

    let scroll_offset = state.logs_scroll.offset;

    // Log area background
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, content_top, content_w, content_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, content_top, content_w, content_h, 12.0);

    // Clip to log area
    ctx.save();
    ctx.clip_rect(margin + 1.0, content_top + 1.0, content_w - 2.0, content_h - 2.0);

    let text_x = margin + 44.0;
    let linenum_x = margin + 10.0;

    for (i, line) in state.log_lines.iter().enumerate() {
        let line_y = content_top + 8.0 + i as f64 * line_h - scroll_offset;

        // Skip invisible
        if line_y + line_h < content_top || line_y > content_bottom {
            continue;
        }

        // Color by content
        let line_color = if line.contains("Error")
            || line.contains("error")
            || line.contains("failed")
            || line.contains("Failed")
        {
            ERROR_COLOR
        } else if line.contains("Connected")
            || line.contains("loaded")
            || line.contains("started")
        {
            SUCCESS
        } else {
            TEXT_PRIMARY
        };

        // Line number
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("11px monospace");
        ctx.set_text_align(TextAlign::Right);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(&format!("{:>3}", i + 1), linenum_x + 24.0, line_y);

        // Separator line between line number and text
        ctx.set_stroke_color(SEPARATOR);
        ctx.set_stroke_width(0.5);
        ctx.begin_path();
        ctx.move_to(margin + 36.0, line_y);
        ctx.line_to(margin + 36.0, line_y + line_h - 2.0);
        ctx.stroke();

        // Line text
        ctx.set_fill_color(line_color);
        ctx.set_font("12px monospace");
        ctx.set_text_align(TextAlign::Left);
        ctx.fill_text(line, text_x, line_y);
    }

    // Empty state
    if state.log_lines.is_empty() {
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px sans-serif");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("No log entries", w / 2.0, content_top + content_h / 2.0);
    }

    ctx.restore();

    // Scrollbar (if content overflows)
    let max_scroll = (total_content_h - content_h).max(0.0);
    if total_content_h > content_h {
        let sb_w = 4.0;
        let sb_x = margin + content_w - sb_w - 4.0;
        let sb_track_h = content_h - 8.0;
        let sb_y = content_top + 4.0;

        let thumb_ratio = content_h / total_content_h;
        let thumb_h = (sb_track_h * thumb_ratio).max(20.0);
        let thumb_offset = if max_scroll > 0.0 {
            (scroll_offset / max_scroll) * (sb_track_h - thumb_h)
        } else {
            0.0
        };

        // Track
        ctx.set_fill_color_alpha(SEPARATOR, 0.4);
        ctx.fill_rounded_rect(sb_x, sb_y, sb_w, sb_track_h, 2.0);

        // Thumb
        ctx.set_fill_color(TEXT_MUTED);
        ctx.fill_rounded_rect(sb_x, sb_y + thumb_offset, sb_w, thumb_h, 2.0);
    }

    // Nav bar
    let active_tab = 3;
    if let Some(tab) = draw_nav_bar(ctx, input, w, h, active_tab) {
        navigate_tab(state, tab);
    }
}
