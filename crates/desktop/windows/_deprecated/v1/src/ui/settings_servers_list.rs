//! Settings → Серверы list screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::Rect;

use crate::app::{AppState, Screen};
use super::components::{
    BG, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED,
    ICON_SERVER,
    draw_back_header, draw_list_item, draw_divider,
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

    if draw_back_header(ctx, input, state, "Серверы", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let list_top = 76.0;
    let item_h = 64.0;
    let content_w = w - margin * 2.0;

    let chain_count = state.config.as_ref().map(|c| c.chains.len()).unwrap_or(0);

    if chain_count == 0 {
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("14px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("Серверы не настроены", w / 2.0, h / 2.0);
        return;
    }

    let list_h = item_h * chain_count as f64;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, list_top, content_w, list_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, list_top, content_w, list_h, 16.0);

    let chain_data: Vec<(String, String)> = state
        .config
        .as_ref()
        .map(|c| {
            c.chains
                .iter()
                .map(|ch| (ch.chain_id.clone(), ch.l1.addr.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let mut navigate_to: Option<usize> = None;

    for (i, (name, addr)) in chain_data.iter().enumerate() {
        let item_y = list_top + i as f64 * item_h;

        let clicked = draw_list_item(
            ctx,
            input,
            state,
            &format!("srv_list_{i}"),
            ICON_SERVER,
            name,
            addr,
            margin,
            item_y,
            content_w,
            item_h,
        );

        if i < chain_data.len().saturating_sub(1) {
            draw_divider(ctx, margin + 52.0, item_y + item_h, content_w - 52.0);
        }

        if clicked {
            navigate_to = Some(i);
        }
    }

    // Suppress TEXT_PRIMARY unused
    let _ = TEXT_PRIMARY;

    if let Some(idx) = navigate_to {
        state.push_screen(Screen::ServerInfo { server_idx: idx });
    }
}
