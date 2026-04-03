//! Servers screen — server list with Amnezia-style selection and scroll.

use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::{Rect, WidgetId};
use uzor::input::{InputState, Sense};

use crate::app::{AppState, Screen};
use super::components::{
    BG, SURFACE, SURFACE_2, ACCENT, TEXT_PRIMARY, TEXT_MUTED, SEPARATOR, NAV_HEIGHT,
    ICON_SETTINGS, ICON_PLUS,
    draw_nav_bar, draw_divider,
};

pub fn draw_servers_screen(
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

    let margin = 16.0;

    // Header "Серверы"
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 32px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Серверы", margin, 20.0);

    let content_top = 72.0;
    let content_bottom = h - NAV_HEIGHT - 8.0;
    let _content_h = content_bottom - content_top;
    let content_w = w - margin * 2.0;

    // Protocol selector pill (placeholder)
    let proto_pill_y = content_top;
    let proto_pill_h = 40.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, proto_pill_y, content_w, proto_pill_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, proto_pill_y, content_w, proto_pill_h, 12.0);

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("VPN протокол: AmneziaWG", margin + 12.0, proto_pill_y + proto_pill_h / 2.0);

    let list_top = proto_pill_y + proto_pill_h + 16.0;
    let item_h = 72.0;

    // Server list area background
    if state.config.is_some() {
        let chain_count = state.config.as_ref().map(|c| c.chains.len()).unwrap_or(0);

        if chain_count == 0 {
            ctx.set_fill_color(TEXT_MUTED);
            ctx.set_font("14px pt root ui");
            ctx.set_text_align(TextAlign::Center);
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.fill_text(
                "Серверы не настроены",
                w / 2.0,
                (list_top + content_bottom) / 2.0,
            );
        } else {
            let list_h = item_h * chain_count as f64;

            // List container
            ctx.set_fill_color(SURFACE);
            ctx.fill_rounded_rect(margin, list_top, content_w, list_h, 16.0);
            ctx.set_stroke_color(SURFACE_2);
            ctx.set_stroke_width(1.0);
            ctx.stroke_rounded_rect(margin, list_top, content_w, list_h, 16.0);

            // Collect chain data
            let chain_data: Vec<(String, String, String, bool)> = state
                .config
                .as_ref()
                .map(|c| {
                    c.chains
                        .iter()
                        .enumerate()
                        .map(|(idx, ch)| {
                            (
                                ch.chain_id.clone(),
                                ch.protocol.to_string(),
                                ch.l1.addr.to_string(),
                                idx == state.selected_chain,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();

            let mut clicked_select = None;
            let mut clicked_settings = None;

            for (idx, (chain_id, protocol, addr, is_selected)) in chain_data.iter().enumerate() {
                let item_y = list_top + idx as f64 * item_h;

                // Row hover
                let row_rect = Rect::new(margin, item_y, content_w - 56.0, item_h);
                let row_id = WidgetId::new(format!("server_row_{idx}"));
                state.coordinator.register(row_id.clone(), row_rect, Sense::CLICK);
                let row_hovered = state.coordinator.is_hovered(&row_id);

                if row_hovered {
                    ctx.set_fill_color_alpha(SURFACE_2, 0.4);
                    ctx.fill_rect(margin, item_y, content_w - 56.0, item_h);
                }

                // Radio indicator (left side)
                let radio_cx = margin + 28.0;
                let radio_cy = item_y + item_h / 2.0;
                let radio_r = 9.0;

                if *is_selected {
                    // Outer ring in accent
                    ctx.begin_path();
                    ctx.arc(radio_cx, radio_cy, radio_r, 0.0, std::f64::consts::PI * 2.0);
                    ctx.set_stroke_color(ACCENT);
                    ctx.set_stroke_width(2.0);
                    ctx.stroke();
                    // Inner dot
                    ctx.begin_path();
                    ctx.arc(radio_cx, radio_cy, radio_r * 0.5, 0.0, std::f64::consts::PI * 2.0);
                    ctx.set_fill_color(ACCENT);
                    ctx.fill();
                } else {
                    ctx.begin_path();
                    ctx.arc(radio_cx, radio_cy, radio_r, 0.0, std::f64::consts::PI * 2.0);
                    ctx.set_stroke_color(SEPARATOR);
                    ctx.set_stroke_width(2.0);
                    ctx.stroke();
                }

                // Server name
                let name_color = if *is_selected { ACCENT } else { TEXT_PRIMARY };
                ctx.set_fill_color(name_color);
                ctx.set_font("bold 16px pt root ui");
                ctx.set_text_align(TextAlign::Left);
                ctx.set_text_baseline(TextBaseline::Bottom);
                ctx.fill_text(chain_id, margin + 52.0, item_y + item_h / 2.0 - 1.0);

                // Protocol + address
                ctx.set_fill_color(TEXT_MUTED);
                ctx.set_font("13px pt root ui");
                ctx.set_text_baseline(TextBaseline::Top);
                let detail = format!("{protocol}  ·  {addr}");
                ctx.fill_text(&detail, margin + 52.0, item_y + item_h / 2.0 + 3.0);

                // Settings gear button (right side)
                let gear_size = 40.0;
                let gear_x = margin + content_w - gear_size - 8.0;
                let gear_y = item_y + (item_h - gear_size) / 2.0;
                let gear_rect = Rect::new(gear_x, gear_y, gear_size, gear_size);
                let gear_id = WidgetId::new(format!("server_gear_{idx}"));
                state.coordinator.register(gear_id.clone(), gear_rect, Sense::CLICK);
                let gear_hovered = state.coordinator.is_hovered(&gear_id);

                if gear_hovered {
                    ctx.set_fill_color_alpha(SURFACE_2, 0.6);
                    ctx.fill_rounded_rect(gear_x, gear_y, gear_size, gear_size, 8.0);
                }

                let icon_sz = 22.0;
                draw_svg_icon(
                    ctx,
                    ICON_SETTINGS,
                    gear_x + (gear_size - icon_sz) / 2.0,
                    gear_y + (gear_size - icon_sz) / 2.0,
                    icon_sz,
                    icon_sz,
                    TEXT_MUTED,
                );

                // Divider between rows
                if idx < chain_data.len().saturating_sub(1) {
                    draw_divider(ctx, margin + 52.0, item_y + item_h, content_w - 52.0 - 8.0);
                }

                if clicked && row_hovered {
                    clicked_select = Some(idx);
                }
                if clicked && gear_hovered {
                    clicked_settings = Some(idx);
                }
            }

            // Handle clicks after iteration (avoid borrow issues)
            if let Some(idx) = clicked_select {
                state.select_chain(idx);
            }
            if let Some(idx) = clicked_settings {
                state.push_screen(Screen::ServerInfo { server_idx: idx });
            }
        }
    } else {
        // No config loaded
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("14px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(
            "Конфигурация не загружена",
            w / 2.0,
            (list_top + content_bottom) / 2.0,
        );
    }

    // "+" add server button at bottom
    let add_btn_w = content_w;
    let add_btn_h = 52.0;
    let add_btn_x = margin;
    let add_btn_y = content_bottom - add_btn_h;

    let add_id = WidgetId::new("servers_add_btn");
    let add_rect = Rect::new(add_btn_x, add_btn_y, add_btn_w, add_btn_h);
    state.coordinator.register(add_id.clone(), add_rect, Sense::CLICK);
    let add_hovered = state.coordinator.is_hovered(&add_id);

    ctx.set_fill_color(if add_hovered { SURFACE_2 } else { SURFACE });
    ctx.fill_rounded_rect(add_btn_x, add_btn_y, add_btn_w, add_btn_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(add_btn_x, add_btn_y, add_btn_w, add_btn_h, 12.0);

    let plus_sz = 20.0;
    draw_svg_icon(
        ctx,
        ICON_PLUS,
        add_btn_x + 16.0,
        add_btn_y + (add_btn_h - plus_sz) / 2.0,
        plus_sz,
        plus_sz,
        ACCENT,
    );
    ctx.set_fill_color(ACCENT);
    ctx.set_font("bold 15px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Добавить сервер", add_btn_x + 16.0 + plus_sz + 10.0, add_btn_y + add_btn_h / 2.0);

    if clicked && add_hovered {
        state.go_to_tab(3); // Go to Connect tab
    }

    // Unused import suppression: use SEPARATOR in divider
    let _ = SEPARATOR;

    // Nav bar
    if let Some(tab) = draw_nav_bar(ctx, input, state, w, h, 1) {
        state.go_to_tab(tab);
    }
}
