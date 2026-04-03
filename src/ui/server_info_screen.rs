//! Server info screen — 3-tab view: Протоколы / Сервисы / Управление.

use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::{Rect, WidgetId};
use uzor::input::{InputState, Sense};

use crate::app::AppState;
use super::components::{
    BG, SURFACE, SURFACE_2, ERROR_COLOR,
    TEXT_PRIMARY, TEXT_MUTED, SEPARATOR, NAV_HEIGHT,
    ICON_CHEVRON_RIGHT, ICON_SETTINGS,
    draw_back_header, draw_tab_bar, draw_divider,
};

pub fn draw_server_info_screen(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    viewport: Rect,
    state: &mut AppState,
    server_idx: usize,
) {
    let w = viewport.width;
    let h = viewport.height;
    let clicked = input.is_clicked();
    let margin = 16.0;

    // Background
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, w, h);

    // Back header
    if draw_back_header(ctx, input, state, "", w) {
        state.pop_screen();
        return;
    }

    // Server name (large, bold)
    let server_name = state
        .config
        .as_ref()
        .and_then(|c| c.chains.get(server_idx))
        .map(|ch| ch.chain_id.as_str())
        .unwrap_or("Сервер");

    let server_addr = state
        .config
        .as_ref()
        .and_then(|c| c.chains.get(server_idx))
        .map(|ch| ch.l1.addr.to_string())
        .unwrap_or_else(|| "—".to_string());

    let name_y = 70.0;
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 32px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(server_name, margin, name_y);

    // Edit icon next to name
    let edit_icon_size = 22.0;
    let edit_x = w - margin - edit_icon_size;
    let edit_y = name_y + 4.0;
    let edit_rect = Rect::new(edit_x - 6.0, edit_y - 6.0, edit_icon_size + 12.0, edit_icon_size + 12.0);
    let edit_id = WidgetId::new("server_edit");
    state.coordinator.register(edit_id.clone(), edit_rect, Sense::CLICK);
    let edit_hovered = state.coordinator.is_hovered(&edit_id);
    let edit_color = if edit_hovered { TEXT_PRIMARY } else { TEXT_MUTED };
    draw_svg_icon(ctx, ICON_SETTINGS, edit_x, edit_y, edit_icon_size, edit_icon_size, edit_color);

    // Subtitle: "root · IP_ADDRESS"
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(&format!("root · {server_addr}"), margin, name_y + 42.0);

    // Tab bar
    let tabs = ["Протоколы", "Сервисы", "Управление"];
    let tab_y = name_y + 70.0;
    let tab_w = w - margin * 2.0;

    if let Some(new_tab) = draw_tab_bar(ctx, input, state, &tabs, state.server_info_tab, margin, tab_y, tab_w) {
        state.server_info_tab = new_tab;
    }

    // Tab content area
    let content_top = tab_y + 48.0;
    let content_h = h - content_top - NAV_HEIGHT - 16.0;
    let content_w = w - margin * 2.0;

    match state.server_info_tab {
        0 => draw_protocols_tab(ctx, input, state, margin, content_top, content_w, content_h, clicked),
        1 => draw_services_tab(ctx, input, state, margin, content_top, content_w, content_h, clicked),
        2 => draw_management_tab(ctx, input, state, margin, content_top, content_w, content_h, clicked),
        _ => {}
    }

    // No nav bar on sub-screens (back button only)
    // Draw a minimal bottom chrome
    ctx.set_fill_color("#1C1D21");
    ctx.fill_rect(0.0, h - NAV_HEIGHT, w, NAV_HEIGHT);
    ctx.set_stroke_color(SEPARATOR);
    ctx.set_stroke_width(0.5);
    ctx.begin_path();
    ctx.move_to(0.0, h - NAV_HEIGHT);
    ctx.line_to(w, h - NAV_HEIGHT);
    ctx.stroke();
}

fn draw_protocols_tab(
    ctx: &mut dyn RenderContext,
    _input: &InputState,
    state: &mut AppState,
    margin: f64,
    y: f64,
    w: f64,
    _h: f64,
    clicked: bool,
) {
    struct ProtocolItem {
        name: &'static str,
        desc: &'static str,
    }
    let protocols = [
        ProtocolItem { name: "AmneziaWG",  desc: "Обфусцированный WireGuard — рекомендуется" },
        ProtocolItem { name: "WireGuard",  desc: "Быстрый современный протокол"              },
        ProtocolItem { name: "XRay",       desc: "XTLS/Reality обфускация"                   },
        ProtocolItem { name: "OpenVPN",    desc: "Классический VPN-протокол"                  },
        ProtocolItem { name: "Cloak",      desc: "Маскировка трафика под HTTPS"               },
    ];

    let item_h = 68.0;
    let list_h = item_h * protocols.len() as f64;

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, w, list_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, w, list_h, 16.0);

    for (i, proto) in protocols.iter().enumerate() {
        let item_y = y + i as f64 * item_h;
        let item_rect = Rect::new(margin, item_y, w, item_h);
        let item_id = WidgetId::new(format!("proto_item_{i}"));
        state.coordinator.register(item_id.clone(), item_rect, Sense::CLICK);
        let is_hovered = state.coordinator.is_hovered(&item_id);

        if is_hovered {
            ctx.set_fill_color_alpha(SURFACE_2, 0.5);
            ctx.fill_rect(margin, item_y, w, item_h);
        }

        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("16px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(proto.name, margin + 16.0, item_y + item_h / 2.0 - 1.0);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(proto.desc, margin + 16.0, item_y + item_h / 2.0 + 3.0);

        let chevron_size = 20.0;
        draw_svg_icon(
            ctx,
            ICON_CHEVRON_RIGHT,
            margin + w - chevron_size - 14.0,
            item_y + (item_h - chevron_size) / 2.0,
            chevron_size,
            chevron_size,
            TEXT_MUTED,
        );

        if i < protocols.len().saturating_sub(1) {
            draw_divider(ctx, margin + 16.0, item_y + item_h, w - 16.0);
        }

        let _ = clicked && is_hovered;
    }
}

fn draw_services_tab(
    ctx: &mut dyn RenderContext,
    _input: &InputState,
    state: &mut AppState,
    margin: f64,
    y: f64,
    w: f64,
    _h: f64,
    clicked: bool,
) {
    struct ServiceItem {
        name: &'static str,
        desc: &'static str,
    }
    let services = [
        ServiceItem { name: "Amnezia DNS", desc: "DNS с защитой от цензуры"     },
        ServiceItem { name: "Tor-сайт",   desc: "Хостинг .onion сайта"          },
    ];

    let item_h = 68.0;
    let list_h = item_h * services.len() as f64;

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, w, list_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, w, list_h, 16.0);

    for (i, svc) in services.iter().enumerate() {
        let item_y = y + i as f64 * item_h;
        let item_rect = Rect::new(margin, item_y, w, item_h);
        let item_id = WidgetId::new(format!("svc_item_{i}"));
        state.coordinator.register(item_id.clone(), item_rect, Sense::CLICK);
        let is_hovered = state.coordinator.is_hovered(&item_id);

        if is_hovered {
            ctx.set_fill_color_alpha(SURFACE_2, 0.5);
            ctx.fill_rect(margin, item_y, w, item_h);
        }

        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("16px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(svc.name, margin + 16.0, item_y + item_h / 2.0 - 1.0);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(svc.desc, margin + 16.0, item_y + item_h / 2.0 + 3.0);

        let chevron_size = 20.0;
        draw_svg_icon(
            ctx,
            ICON_CHEVRON_RIGHT,
            margin + w - chevron_size - 14.0,
            item_y + (item_h - chevron_size) / 2.0,
            chevron_size,
            chevron_size,
            TEXT_MUTED,
        );

        if i < services.len().saturating_sub(1) {
            draw_divider(ctx, margin + 16.0, item_y + item_h, w - 16.0);
        }

        let _ = clicked && is_hovered;
    }
}

fn draw_management_tab(
    ctx: &mut dyn RenderContext,
    _input: &InputState,
    state: &mut AppState,
    margin: f64,
    y: f64,
    w: f64,
    _h: f64,
    clicked: bool,
) {
    // Normal action
    let scan_h = 56.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, w, scan_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, w, scan_h, 12.0);

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("15px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(
        "Проверить ранее установленные сервисы",
        margin + 16.0,
        y + scan_h / 2.0,
    );

    // Destructive actions
    struct DestructiveItem {
        label: &'static str,
        id: &'static str,
    }
    let destructive = [
        DestructiveItem { label: "Перезагрузить сервер",              id: "mgmt_reboot"  },
        DestructiveItem { label: "Удалить сервер из приложения",       id: "mgmt_remove"  },
        DestructiveItem { label: "Очистить сервер от ПО Amnezia",     id: "mgmt_clean"   },
    ];

    let gap = 12.0;
    let dest_h = 56.0;

    for (i, item) in destructive.iter().enumerate() {
        let item_y = y + scan_h + gap + i as f64 * (dest_h + gap);
        let rect = Rect::new(margin, item_y, w, dest_h);
        let widget_id = WidgetId::new(item.id);
        state.coordinator.register(widget_id.clone(), rect, Sense::CLICK);
        let is_hovered = state.coordinator.is_hovered(&widget_id);

        let bg_color = if is_hovered { "#3A1A1A" } else { SURFACE };
        ctx.set_fill_color(bg_color);
        ctx.fill_rounded_rect(margin, item_y, w, dest_h, 12.0);
        ctx.set_stroke_color(if is_hovered { ERROR_COLOR } else { SURFACE_2 });
        ctx.set_stroke_width(1.0);
        ctx.stroke_rounded_rect(margin, item_y, w, dest_h, 12.0);

        ctx.set_fill_color(ERROR_COLOR);
        ctx.set_font("15px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(item.label, margin + 16.0, item_y + dest_h / 2.0);

        let _ = clicked && is_hovered;
    }

    // Suppress unused variable warning
    let _ = SEPARATOR;
}
