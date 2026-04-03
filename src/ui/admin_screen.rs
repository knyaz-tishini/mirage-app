//! Admin panel screen — only visible when `state.admin_unlocked` is true.

use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::{Rect, WidgetId};
use uzor::input::{InputState, Sense};

use crate::app::AppState;
use crate::api::ApiRequest;
use super::components::{
    BG, SURFACE, SURFACE_2, ACCENT, ERROR_COLOR, SUCCESS,
    TEXT_PRIMARY, TEXT_MUTED, NAV_HEIGHT,
    ICON_SERVER, ICON_SETTINGS, ICON_ARROW_LEFT,
    draw_back_header, draw_text_field, draw_divider,
};

/// Draw the admin panel screen.
pub fn draw_admin_screen(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    viewport: Rect,
    state: &mut AppState,
) {
    let w = viewport.width;
    let h = viewport.height;
    let margin = 16.0;
    let content_w = w - margin * 2.0;

    // Background
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, w, h);

    // Back header (returns to previous screen when clicking back)
    let back_clicked = draw_back_header(ctx, input, state, "Панель управления", w);
    if back_clicked {
        state.pop_screen();
        return;
    }

    let mut y = 80.0;

    if !state.admin_unlocked {
        draw_api_key_input(ctx, input, state, margin, content_w, w, &mut y);
    } else {
        draw_admin_panel(ctx, input, state, margin, content_w, w, h, &mut y);
    }
}

// ---------------------------------------------------------------------------
// API key input (shown when not yet unlocked)
// ---------------------------------------------------------------------------

fn draw_api_key_input(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    margin: f64,
    content_w: f64,
    w: f64,
    y: &mut f64,
) {
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("14px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Введите API-ключ для доступа к панели управления", margin, *y);
    *y += 32.0;

    draw_text_field(
        ctx,
        input,
        state,
        "admin_api_key",
        "Bearer token...",
        "",
        margin,
        *y,
        content_w,
        52.0,
    );
    *y += 62.0;

    let btn_h = 48.0;
    let btn_rect = Rect::new(margin, *y, content_w, btn_h);
    let btn_id = WidgetId::new("admin_submit_key");
    state.coordinator.register(btn_id.clone(), btn_rect, Sense::CLICK);
    let btn_hovered = state.coordinator.is_hovered(&btn_id);
    let clicked = input.is_clicked();

    let btn_bg = if btn_hovered { "#f5a550" } else { ACCENT };
    ctx.set_fill_color(btn_bg);
    ctx.fill_rounded_rect(margin, *y, content_w, btn_h, 10.0);
    ctx.set_fill_color(BG);
    ctx.set_font("bold 15px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Войти", w / 2.0, *y + btn_h / 2.0);

    if clicked && btn_hovered {
        let key = state.field_value("admin_api_key").to_string();
        if !key.is_empty() {
            state.api_token = Some(key.clone());
            state.api.send(ApiRequest::SetToken(key));
            state.admin_unlocked = true;
        }
    }

    *y += btn_h;
}

// ---------------------------------------------------------------------------
// Admin panel content (shown when unlocked)
// ---------------------------------------------------------------------------

fn draw_admin_panel(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    margin: f64,
    content_w: f64,
    w: f64,
    h: f64,
    y: &mut f64,
) {
    let clicked = input.is_clicked();

    // Server status badge
    let status_text = if state.server_online { "Сервер онлайн" } else { "Сервер офлайн" };
    let status_color = if state.server_online { SUCCESS } else { ERROR_COLOR };
    let dot_x = margin;
    let dot_y = *y + 6.0;

    ctx.set_fill_color(status_color);
    ctx.begin_path();
    ctx.arc(dot_x + 6.0, dot_y, 5.0, 0.0, 2.0 * std::f64::consts::PI);
    ctx.fill();

    ctx.set_fill_color(status_color);
    ctx.set_font("13px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(status_text, dot_x + 16.0, *y);
    *y += 28.0;

    // Refresh button
    let refresh_h = 40.0;
    let refresh_rect = Rect::new(margin, *y, content_w, refresh_h);
    let refresh_id = WidgetId::new("admin_refresh");
    state.coordinator.register(refresh_id.clone(), refresh_rect, Sense::CLICK);
    let refresh_hovered = state.coordinator.is_hovered(&refresh_id);

    let refresh_bg = if refresh_hovered { SURFACE_2 } else { SURFACE };
    ctx.set_fill_color(refresh_bg);
    ctx.fill_rounded_rect(margin, *y, content_w, refresh_h, 8.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, *y, content_w, refresh_h, 8.0);
    ctx.set_fill_color(ACCENT);
    ctx.set_font("bold 13px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Обновить", w / 2.0, *y + refresh_h / 2.0);

    if clicked && refresh_hovered {
        state.api.send(ApiRequest::FetchNodes);
        state.api.send(ApiRequest::FetchChains);
        state.push_log("Обновление данных...");
    }

    *y += refresh_h + 16.0;

    // Section header
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("СЕРВЕРЫ", margin, *y);
    *y += 20.0;

    // Node list
    if state.nodes.is_empty() {
        ctx.set_fill_color(SURFACE);
        ctx.fill_rounded_rect(margin, *y, content_w, 64.0, 12.0);
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("14px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("Нет серверов", w / 2.0, *y + 32.0);
        *y += 64.0;
    } else {
        let nodes = state.nodes.clone();
        let node_count = nodes.len();

        // Draw container
        let node_h = 80.0;
        let list_h = node_h * node_count as f64;
        ctx.set_fill_color(SURFACE);
        ctx.fill_rounded_rect(margin, *y, content_w, list_h, 12.0);
        ctx.set_stroke_color(SURFACE_2);
        ctx.set_stroke_width(1.0);
        ctx.stroke_rounded_rect(margin, *y, content_w, list_h, 12.0);

        let list_top = *y;

        for (i, node) in nodes.iter().enumerate() {
            let node_y = list_top + i as f64 * node_h;

            // Row hover
            let row_rect = Rect::new(margin, node_y, content_w, node_h);
            let row_id = WidgetId::new(format!("admin_node_{i}"));
            state.coordinator.register(row_id.clone(), row_rect, Sense::CLICK);
            let row_hovered = state.coordinator.is_hovered(&row_id);

            if row_hovered {
                ctx.set_fill_color_alpha(SURFACE_2, 0.4);
                ctx.fill_rect(margin, node_y, content_w, node_h);
            }

            // Server icon
            let icon_size = 20.0;
            draw_svg_icon(ctx, ICON_SERVER, margin + 14.0, node_y + (node_h - icon_size) / 2.0, icon_size, icon_size, TEXT_MUTED);

            // Node name and IP
            let text_x = margin + 14.0 + icon_size + 12.0;
            ctx.set_fill_color(TEXT_PRIMARY);
            ctx.set_font("bold 14px pt root ui");
            ctx.set_text_align(TextAlign::Left);
            ctx.set_text_baseline(TextBaseline::Bottom);
            ctx.fill_text(&node.name, text_x, node_y + node_h / 2.0 - 2.0);

            let ip_label = format!("{} · {}", node.ip, node.role);
            ctx.set_fill_color(TEXT_MUTED);
            ctx.set_font("12px pt root ui");
            ctx.set_text_baseline(TextBaseline::Top);
            ctx.fill_text(&ip_label, text_x, node_y + node_h / 2.0 + 4.0);

            // Status badge
            let (badge_color, badge_text) = match node.status.as_str() {
                "active" | "online" => (SUCCESS, "online"),
                "deploying" => (ACCENT, "deploying"),
                _ => (ERROR_COLOR, "offline"),
            };
            ctx.set_fill_color(badge_color);
            ctx.set_font("11px pt root ui");
            ctx.set_text_align(TextAlign::Right);
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.fill_text(badge_text, margin + content_w - 80.0, node_y + node_h / 2.0);

            // Reboot button
            let reboot_w = 64.0;
            let reboot_h = 28.0;
            let reboot_x = margin + content_w - reboot_w - 10.0;
            let reboot_y = node_y + (node_h - reboot_h) / 2.0;
            let reboot_rect = Rect::new(reboot_x, reboot_y, reboot_w, reboot_h);
            let reboot_id = WidgetId::new(format!("admin_reboot_{i}"));
            state.coordinator.register(reboot_id.clone(), reboot_rect, Sense::CLICK);
            let reboot_hovered = state.coordinator.is_hovered(&reboot_id);

            let reboot_bg = if reboot_hovered { "#3A1A1A" } else { SURFACE_2 };
            ctx.set_fill_color(reboot_bg);
            ctx.fill_rounded_rect(reboot_x, reboot_y, reboot_w, reboot_h, 6.0);
            ctx.set_fill_color(ERROR_COLOR);
            ctx.set_font("11px pt root ui");
            ctx.set_text_align(TextAlign::Center);
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.fill_text("Reboot", reboot_x + reboot_w / 2.0, reboot_y + reboot_h / 2.0);

            if clicked && reboot_hovered {
                let node_id = node.id;
                state.api.send(ApiRequest::RebootNode { node_id });
                state.push_log(&format!("Перезагрузка узла {}...", node.name));
            }

            // Divider between nodes
            if i < node_count.saturating_sub(1) {
                draw_divider(ctx, margin + 12.0, node_y + node_h, content_w - 12.0);
            }
        }

        *y += list_h + 16.0;
    }

    // Ensure we don't draw below nav bar
    let _max_y = h - NAV_HEIGHT - 8.0;
    let _ = w;

    // Settings icon for admin panel (gear icon in top-right)
    let gear_size = 20.0;
    let gear_x = w - margin - gear_size;
    let gear_rect = Rect::new(gear_x - 6.0, 24.0, gear_size + 12.0, gear_size + 12.0);
    let gear_id = WidgetId::new("admin_settings");
    state.coordinator.register(gear_id.clone(), gear_rect, Sense::CLICK);
    let gear_hovered = state.coordinator.is_hovered(&gear_id);
    let gear_color = if gear_hovered { TEXT_PRIMARY } else { TEXT_MUTED };
    draw_svg_icon(ctx, ICON_SETTINGS, gear_x, 30.0, gear_size, gear_size, gear_color);

    // Unused import suppress — ICON_ARROW_LEFT is re-exported from components
    let _ = ICON_ARROW_LEFT;
}
