//! Chains screen — server list with Amnezia-style selection and scroll.

use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::{Rect, WidgetId};
use uzor::input::{InputState, Sense};

use crate::app::{AppState, Screen};
use super::components::{
    BG, SURFACE, SURFACE_2, ACCENT, TEXT_PRIMARY, TEXT_MUTED, SEPARATOR, NAV_HEIGHT,
    ICON_CHECK,
    draw_nav_bar,
};
use super::main_screen::navigate_tab;

pub fn draw_chains_screen(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    viewport: Rect,
    state: &mut AppState,
) {
    let w = viewport.width;
    let h = viewport.height;
    let clicked = input.is_clicked();

    // Background
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, w, h);

    // Title
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 18px sans-serif");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Server Chains", w / 2.0, 24.0);

    let content_top = 64.0;
    let content_bottom = h - NAV_HEIGHT - 8.0;
    let content_h = content_bottom - content_top;

    let item_h = 70.0;
    let item_gap = 8.0;
    let item_margin = 16.0;
    let item_w = w - item_margin * 2.0;

    if let Some(config) = &state.config {
        if config.chains.is_empty() {
            ctx.set_fill_color(TEXT_MUTED);
            ctx.set_font("14px sans-serif");
            ctx.set_text_align(TextAlign::Center);
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.fill_text("No chains in config", w / 2.0, (content_top + content_bottom) / 2.0);
        } else {
            let chain_count = config.chains.len();
            let total_content_h = chain_count as f64 * (item_h + item_gap);

            // Handle scroll wheel — raw is_hovered for the scroll area is fine (no z-order
            // conflict here, the scroll area is the only widget in this region)
            let scroll_delta = input.scroll_delta.1;
            if scroll_delta != 0.0 {
                let scroll_area = Rect::new(0.0, content_top, w, content_h);
                if input.is_hovered(&scroll_area) {
                    state
                        .chains_scroll
                        .handle_wheel(scroll_delta, total_content_h, content_h);
                }
            }

            let scroll_offset = state.chains_scroll.offset;

            let selected = state.selected_chain;

            // Collect chain data before mutable borrow on state
            let chain_data: Vec<(String, String, String, bool)> = config
                .chains
                .iter()
                .enumerate()
                .map(|(idx, ch)| {
                    (
                        ch.chain_id.clone(),
                        ch.protocol.to_string(),
                        ch.l1.addr.to_string(),
                        idx == selected,
                    )
                })
                .collect();

            // Clip to content area
            ctx.save();
            ctx.clip_rect(0.0, content_top, w, content_h);

            let mut clicked_idx = None;

            for (idx, (chain_id, protocol, addr, is_selected)) in chain_data.iter().enumerate() {
                let item_y = content_top + idx as f64 * (item_h + item_gap) - scroll_offset;

                // Skip if outside visible area
                if item_y + item_h < content_top || item_y > content_bottom {
                    continue;
                }

                let item_rect = Rect::new(item_margin, item_y, item_w, item_h);
                let item_id = WidgetId::new(format!("chain_{idx}"));

                state.coordinator.register(item_id.clone(), item_rect, Sense::CLICK);
                let is_hovered = state.coordinator.is_hovered(&item_id);

                // Card background — slightly lighter on hover
                let card_fill = if is_hovered && !is_selected {
                    SURFACE_2
                } else {
                    SURFACE
                };
                ctx.set_fill_color(card_fill);
                ctx.fill_rounded_rect(item_margin, item_y, item_w, item_h, 12.0);

                // Border: 1px goldenApricot if selected, 1px slateGray if not
                let border_color = if *is_selected { ACCENT } else { SURFACE_2 };
                ctx.set_stroke_color(border_color);
                ctx.set_stroke_width(1.0);
                ctx.stroke_rounded_rect(item_margin, item_y, item_w, item_h, 12.0);

                // Checkmark or empty circle indicator
                if *is_selected {
                    let check_size = 18.0;
                    draw_svg_icon(
                        ctx,
                        ICON_CHECK,
                        item_margin + 12.0,
                        item_y + (item_h - check_size) / 2.0,
                        check_size,
                        check_size,
                        ACCENT,
                    );
                } else {
                    // Small empty circle
                    ctx.begin_path();
                    ctx.arc(
                        item_margin + 21.0,
                        item_y + item_h / 2.0,
                        7.0,
                        0.0,
                        std::f64::consts::PI * 2.0,
                    );
                    ctx.set_stroke_color(SEPARATOR);
                    ctx.set_stroke_width(1.0);
                    ctx.stroke();
                }

                // Chain name
                let name_color = if *is_selected { TEXT_PRIMARY } else { TEXT_MUTED };
                ctx.set_fill_color(name_color);
                ctx.set_font("bold 15px sans-serif");
                ctx.set_text_align(TextAlign::Left);
                ctx.set_text_baseline(TextBaseline::Top);
                ctx.fill_text(chain_id, item_margin + 44.0, item_y + 16.0);

                // Protocol + addr
                ctx.set_fill_color(TEXT_MUTED);
                ctx.set_font("12px sans-serif");
                ctx.set_text_baseline(TextBaseline::Top);
                let detail = format!("{protocol}  ·  {addr}");
                ctx.fill_text(&detail, item_margin + 44.0, item_y + 36.0);

                // Click detection
                if clicked && is_hovered {
                    clicked_idx = Some(idx);
                }
            }

            ctx.restore();

            // Scrollbar (if content overflows)
            let max_scroll = (total_content_h - content_h).max(0.0);
            if total_content_h > content_h {
                let sb_w = 4.0;
                let sb_x = w - sb_w - 4.0;
                let sb_track_h = content_h - 8.0;
                let sb_y = content_top + 4.0;

                let thumb_ratio = content_h / total_content_h;
                let thumb_h = (sb_track_h * thumb_ratio).max(20.0);
                let thumb_offset = if max_scroll > 0.0 {
                    (scroll_offset / max_scroll) * (sb_track_h - thumb_h)
                } else {
                    0.0
                };

                ctx.set_fill_color_alpha(SEPARATOR, 0.4);
                ctx.fill_rounded_rect(sb_x, sb_y, sb_w, sb_track_h, 2.0);

                ctx.set_fill_color(TEXT_MUTED);
                ctx.fill_rounded_rect(sb_x, sb_y + thumb_offset, sb_w, thumb_h, 2.0);
            }

            if let Some(idx) = clicked_idx {
                state.select_chain(idx);
            }
        }
    } else {
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("14px sans-serif");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(
            "Config not loaded",
            w / 2.0,
            (content_top + content_bottom) / 2.0 - 20.0,
        );

        // Button to go to settings
        let btn_w = 180.0;
        let btn_h = 40.0;
        let btn_x = (w - btn_w) / 2.0;
        let btn_y = (content_top + content_bottom) / 2.0 + 10.0;

        ctx.set_fill_color(ACCENT);
        ctx.fill_rounded_rect(btn_x, btn_y, btn_w, btn_h, 8.0);

        ctx.set_fill_color(BG);
        ctx.set_font("bold 14px sans-serif");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("Open Settings", w / 2.0, btn_y + btn_h / 2.0);

        let settings_btn_id = WidgetId::new("chains_settings_btn");
        let btn_rect = Rect::new(btn_x, btn_y, btn_w, btn_h);
        state.coordinator.register(settings_btn_id.clone(), btn_rect, Sense::CLICK);

        if clicked && state.coordinator.is_hovered(&settings_btn_id) {
            state.go_to(Screen::Settings);
        }
    }

    // Nav bar
    let active_tab = 1;
    if let Some(tab) = draw_nav_bar(ctx, input, state, w, h, active_tab) {
        navigate_tab(state, tab);
    }
}
