//! Settings screen — Amnezia-style settings list.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;

use crate::app::AppState;
use super::components::{
    BG, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED, NAV_HEIGHT,
    ICON_SERVER, ICON_SETTINGS, ICON_LOGS,
    draw_nav_bar, draw_divider, draw_settings_row,
};
use super::main_screen::navigate_tab;

pub fn draw_settings_screen(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    viewport: uzor::types::Rect,
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
    ctx.fill_text("Settings", w / 2.0, 24.0);

    let margin = 16.0;
    let content_x = margin;
    let content_w = w - margin * 2.0;
    let row_h = 56.0;
    let list_top = 68.0;

    // Settings list card container
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(content_x, list_top, content_w, row_h * 5.0, 16.0);

    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(content_x, list_top, content_w, row_h * 5.0, 16.0);

    // Settings items
    struct Item {
        icon: &'static str,
        title: &'static str,
        id: &'static str,
    }

    let items: &[Item] = &[
        Item { icon: ICON_SERVER,   title: "Servers",     id: "settings_row_0" },
        Item { icon: ICON_SETTINGS, title: "Connection",  id: "settings_row_1" },
        Item { icon: ICON_SETTINGS, title: "Application", id: "settings_row_2" },
        Item { icon: ICON_LOGS,     title: "Logs",        id: "settings_row_3" },
        Item { icon: ICON_SETTINGS, title: "About",       id: "settings_row_4" },
    ];

    for (i, item) in items.iter().enumerate() {
        let row_y = list_top + i as f64 * row_h;

        let _row_clicked = draw_settings_row(
            ctx,
            input,
            state,
            item.id,
            item.icon,
            item.title,
            content_x,
            row_y,
            content_w,
            row_h,
        );

        // Divider between items (not after last)
        if i < items.len().saturating_sub(1) {
            draw_divider(ctx, content_x + 52.0, row_y + row_h, content_w - 52.0);
        }
    }

    // Config info card below list
    let info_y = list_top + row_h * items.len() as f64 + 20.0;
    let info_h = if state.config.is_some() { 88.0 } else { 44.0 };

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(content_x, info_y, content_w, info_h, 12.0);

    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(content_x, info_y, content_w, info_h, 12.0);

    if let Some(config) = &state.config {
        let lines = [
            format!("Client ID: {}", config.client_id),
            format!("Chains: {}", config.chains.len()),
            format!("Direct CIDRs: {}", config.routing.direct_cidrs.len()),
            format!("Blocked domains: {}", config.routing.blocked_domains.len()),
        ];
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("12px sans-serif");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
        for (i, line) in lines.iter().enumerate() {
            ctx.fill_text(line, content_x + 16.0, info_y + 10.0 + i as f64 * 18.0);
        }
    } else {
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px sans-serif");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("Config not loaded", content_x + content_w / 2.0, info_y + info_h / 2.0);
    }

    // SOCKS5 section
    let socks_y = info_y + info_h + 20.0;
    let socks_h = 48.0;

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("SOCKS5 PROXY", content_x, socks_y - 18.0);

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(content_x, socks_y, content_w, socks_h, 12.0);

    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(content_x, socks_y, content_w, socks_h, 12.0);

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("13px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(&state.socks5_addr, content_x + 16.0, socks_y + socks_h / 2.0);

    // Config path (small display)
    let path_y = socks_y + socks_h + 8.0;
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    let display_path = truncate_path(&state.config_path, 40);
    ctx.fill_text(&display_path, content_x, path_y);

    // Version footer
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px sans-serif");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Bottom);
    ctx.fill_text("Mirage  v0.1.0", w / 2.0, h - NAV_HEIGHT - 12.0);

    // Nav bar
    let active_tab = 2;
    if let Some(tab) = draw_nav_bar(ctx, input, state, w, h, active_tab) {
        navigate_tab(state, tab);
    }
}

fn truncate_path(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        path.to_string()
    } else {
        let tail: String = path
            .chars()
            .rev()
            .take(max_chars.saturating_sub(3))
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("...{tail}")
    }
}
