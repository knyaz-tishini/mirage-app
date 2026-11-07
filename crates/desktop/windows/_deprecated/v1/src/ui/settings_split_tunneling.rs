//! Settings → Split tunneling screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::{Rect, WidgetId};
use uzor::input::Sense;

use crate::app::{AppState, ConfirmDrawer, SplitTunnelingMode};
use super::components::{
    BG, SURFACE, SURFACE_2, ACCENT, TEXT_PRIMARY, TEXT_MUTED, ERROR_COLOR,
    draw_back_header, draw_toggle, draw_divider, draw_text_field,
};
use std::f64::consts::PI;

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

    if draw_back_header(ctx, input, state, "Раздельное туннелирование", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let content_w = w - margin * 2.0;
    let mut y = 80.0;
    let clicked = input.is_clicked();

    // ── Enable toggle ──────────────────────────────────────────────────────
    let toggle_row_h = 60.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, toggle_row_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, toggle_row_h, 12.0);

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("15px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Bottom);
    ctx.fill_text("Включить", margin + 16.0, y + toggle_row_h / 2.0 - 1.0);

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("12px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Выборочный маршрут трафика", margin + 16.0, y + toggle_row_h / 2.0 + 3.0);

    let pill_w = 44.0;
    let pill_h = 26.0;
    let pill_x = margin + content_w - pill_w - 16.0;
    let pill_y = y + (toggle_row_h - pill_h) / 2.0;

    // Sync toggle with split_tunneling_enabled
    if state.toggle_get("split_tunneling_enabled") != state.split_tunneling_enabled {
        state.toggles.insert("split_tunneling_enabled".into(), state.split_tunneling_enabled);
    }
    if draw_toggle(ctx, input, state, "split_tunneling_enabled", pill_x, pill_y) {
        state.split_tunneling_enabled = state.toggle_get("split_tunneling_enabled");
    }

    y += toggle_row_h + 16.0;

    if state.split_tunneling_enabled {
        // ── Mode selector ──────────────────────────────────────────────────
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("11px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("РЕЖИМ", margin + 4.0, y);
        y += 18.0;

        let mode_row_h = 52.0;
        ctx.set_fill_color(SURFACE);
        ctx.fill_rounded_rect(margin, y, content_w, mode_row_h * 2.0, 12.0);
        ctx.set_stroke_color(SURFACE_2);
        ctx.set_stroke_width(1.0);
        ctx.stroke_rounded_rect(margin, y, content_w, mode_row_h * 2.0, 12.0);

        let is_forward = matches!(state.split_tunneling_mode, SplitTunnelingMode::ForwardOnly);
        let is_block = matches!(state.split_tunneling_mode, SplitTunnelingMode::BlockListed);

        let row1_rect = Rect::new(margin, y, content_w, mode_row_h);
        let row1_id = WidgetId::new("split_mode_forward");
        state.coordinator.register(row1_id.clone(), row1_rect, Sense::CLICK);
        let row1_hovered = state.coordinator.is_hovered(&row1_id);

        if row1_hovered {
            ctx.set_fill_color_alpha(SURFACE_2, 0.3);
            ctx.fill_rounded_rect(margin, y, content_w, mode_row_h, 12.0);
        }

        draw_radio_dot(ctx, is_forward, margin + content_w - 26.0, y + mode_row_h / 2.0);

        ctx.set_fill_color(if is_forward { TEXT_PRIMARY } else { TEXT_MUTED });
        ctx.set_font("14px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("Только выбранные сайты", margin + 16.0, y + mode_row_h / 2.0);

        if clicked && row1_hovered {
            state.split_tunneling_mode = SplitTunnelingMode::ForwardOnly;
        }

        draw_divider(ctx, margin + 16.0, y + mode_row_h, content_w - 16.0);

        let row2_y = y + mode_row_h;
        let row2_rect = Rect::new(margin, row2_y, content_w, mode_row_h);
        let row2_id = WidgetId::new("split_mode_block");
        state.coordinator.register(row2_id.clone(), row2_rect, Sense::CLICK);
        let row2_hovered = state.coordinator.is_hovered(&row2_id);

        if row2_hovered {
            ctx.set_fill_color_alpha(SURFACE_2, 0.3);
            ctx.fill_rounded_rect(margin, row2_y, content_w, mode_row_h, 12.0);
        }

        draw_radio_dot(ctx, is_block, margin + content_w - 26.0, row2_y + mode_row_h / 2.0);

        ctx.set_fill_color(if is_block { TEXT_PRIMARY } else { TEXT_MUTED });
        ctx.set_font("14px pt root ui");
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("Все кроме выбранных", margin + 16.0, row2_y + mode_row_h / 2.0);

        if clicked && row2_hovered {
            state.split_tunneling_mode = SplitTunnelingMode::BlockListed;
        }

        y += mode_row_h * 2.0 + 16.0;

        // ── Site list ──────────────────────────────────────────────────────
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("11px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("САЙТЫ", margin + 4.0, y);
        y += 18.0;

        let site_row_h = 48.0;
        let sites_count = state.split_tunneling_sites.len();

        // Collect any site removal actions
        let mut remove_site_idx: Option<usize> = None;

        if sites_count > 0 {
            let list_h = site_row_h * sites_count as f64;
            ctx.set_fill_color(SURFACE);
            ctx.fill_rounded_rect(margin, y, content_w, list_h, 12.0);
            ctx.set_stroke_color(SURFACE_2);
            ctx.set_stroke_width(1.0);
            ctx.stroke_rounded_rect(margin, y, content_w, list_h, 12.0);

            for i in 0..sites_count {
                let site = state.split_tunneling_sites[i].clone();
                let site_y = y + i as f64 * site_row_h;
                let mid_y = site_y + site_row_h / 2.0;

                ctx.set_fill_color(TEXT_PRIMARY);
                ctx.set_font("14px pt root ui");
                ctx.set_text_align(TextAlign::Left);
                ctx.set_text_baseline(TextBaseline::Middle);
                ctx.fill_text(&site, margin + 16.0, mid_y);

                // X remove button
                let xbtn_size = 28.0;
                let xbtn_x = margin + content_w - xbtn_size - 14.0;
                let xbtn_y = mid_y - xbtn_size / 2.0;
                let xbtn_id = WidgetId::new(format!("split_remove_{i}"));
                let xbtn_rect = Rect::new(xbtn_x, xbtn_y, xbtn_size, xbtn_size);
                state.coordinator.register(xbtn_id.clone(), xbtn_rect, Sense::CLICK);
                let xbtn_hovered = state.coordinator.is_hovered(&xbtn_id);

                if xbtn_hovered {
                    ctx.set_fill_color_alpha(ERROR_COLOR, 0.15);
                    ctx.fill_rounded_rect(xbtn_x, xbtn_y, xbtn_size, xbtn_size, 6.0);
                }

                // Draw X symbol
                ctx.set_fill_color(if xbtn_hovered { ERROR_COLOR } else { TEXT_MUTED });
                ctx.set_font("bold 16px pt root ui");
                ctx.set_text_align(TextAlign::Center);
                ctx.set_text_baseline(TextBaseline::Middle);
                ctx.fill_text("×", xbtn_x + xbtn_size / 2.0, mid_y);

                if clicked && xbtn_hovered {
                    remove_site_idx = Some(i);
                }

                if i < sites_count.saturating_sub(1) {
                    draw_divider(ctx, margin + 16.0, site_y + site_row_h, content_w - 16.0);
                }
            }

            y += list_h + 12.0;
        } else {
            let empty_h = 52.0;
            ctx.set_fill_color(SURFACE);
            ctx.fill_rounded_rect(margin, y, content_w, empty_h, 12.0);
            ctx.set_stroke_color(SURFACE_2);
            ctx.set_stroke_width(1.0);
            ctx.stroke_rounded_rect(margin, y, content_w, empty_h, 12.0);

            ctx.set_fill_color(TEXT_MUTED);
            ctx.set_font("14px pt root ui");
            ctx.set_text_align(TextAlign::Center);
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.fill_text("Список сайтов пуст", margin + content_w / 2.0, y + empty_h / 2.0);

            y += empty_h + 12.0;
        }

        // Apply pending removal via confirm drawer
        if let Some(idx) = remove_site_idx {
            state.show_confirm(ConfirmDrawer {
                title: "Удалить сайт?".into(),
                description: format!(
                    "\"{}\" будет удалён из списка.",
                    state.split_tunneling_sites.get(idx).cloned().unwrap_or_default()
                ),
                confirm_label: "Удалить".into(),
                cancel_label: "Отмена".into(),
                action_id: format!("delete_site_{idx}"),
            });
        }

        // ── Add site field + button ────────────────────────────────────────
        let field_h = 48.0;
        let field_w = content_w - 60.0;
        draw_text_field(
            ctx, input, state,
            "split_add_site",
            "Добавить сайт...",
            "",
            margin, y, field_w, field_h,
        );

        let add_btn_w = 48.0;
        let add_btn_x = margin + field_w + 8.0;
        let add_id = WidgetId::new("split_add_btn");
        let add_rect = Rect::new(add_btn_x, y, add_btn_w, field_h);
        state.coordinator.register(add_id.clone(), add_rect, Sense::CLICK);
        let add_hovered = state.coordinator.is_hovered(&add_id);

        let add_bg = if add_hovered { ACCENT } else { SURFACE_2 };
        ctx.set_fill_color(add_bg);
        ctx.fill_rounded_rect(add_btn_x, y, add_btn_w, field_h, 10.0);

        ctx.set_fill_color(if add_hovered { BG } else { TEXT_PRIMARY });
        ctx.set_font("bold 22px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("+", add_btn_x + add_btn_w / 2.0, y + field_h / 2.0);

        if clicked && add_hovered {
            let new_site = state.field_value("split_add_site").trim().to_string();
            if !new_site.is_empty() && !state.split_tunneling_sites.contains(&new_site) {
                state.split_tunneling_sites.push(new_site);
                state.field_set_text("split_add_site", String::new());
                state.blur_field();
            }
        }
    } else {
        // ── Disabled description ───────────────────────────────────────────
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("Позволяет направлять трафик отдельных сайтов", margin, y);
        ctx.fill_text("в обход VPN или только через VPN.", margin, y + 20.0);
    }

    let _ = h;
}

fn draw_radio_dot(ctx: &mut dyn RenderContext, selected: bool, cx: f64, cy: f64) {
    let r = 10.0;
    if selected {
        ctx.set_fill_color(ACCENT);
        ctx.begin_path();
        ctx.arc(cx, cy, r, 0.0, 2.0 * PI);
        ctx.fill();
        ctx.set_fill_color("#FFFFFF");
        ctx.begin_path();
        ctx.arc(cx, cy, 4.0, 0.0, 2.0 * PI);
        ctx.fill();
    } else {
        ctx.set_stroke_color(SURFACE_2);
        ctx.set_stroke_width(1.5);
        ctx.begin_path();
        ctx.arc(cx, cy, r, 0.0, 2.0 * PI);
        ctx.stroke();
    }
}

