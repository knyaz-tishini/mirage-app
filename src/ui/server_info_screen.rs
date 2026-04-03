//! Server info screen — 3-tab view: Протоколы / Сервисы / Управление.

use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::{Rect, WidgetId};
use uzor::input::{InputState, Sense};

use crate::app::{AppState, ConfirmDrawer, Screen};
use super::components::{
    BG, SURFACE, SURFACE_2, ACCENT, ERROR_COLOR,
    TEXT_PRIMARY, TEXT_MUTED, NAV_HEIGHT,
    ICON_CHEVRON_RIGHT, ICON_SETTINGS,
    draw_back_header, draw_tab_bar, draw_divider, draw_toggle,
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
        .map(|ch| ch.chain_id.clone())
        .unwrap_or_else(|| "Сервер".to_string());

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
    ctx.fill_text(&server_name, margin, name_y);

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
        0 => draw_protocols_tab(ctx, input, state, margin, content_top, content_w, content_h, clicked, server_idx),
        1 => draw_services_tab(ctx, input, state, margin, content_top, content_w, content_h, clicked),
        2 => draw_management_tab(ctx, input, state, margin, content_top, content_w, content_h, clicked, server_idx),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tab 0: Протоколы
// ---------------------------------------------------------------------------

fn draw_protocols_tab(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    state: &mut AppState,
    margin: f64,
    y: f64,
    w: f64,
    _h: f64,
    clicked: bool,
    chain_idx: usize,
) {
    struct ProtocolItem {
        name: &'static str,
        desc: &'static str,
        toggle_id: &'static str,
        /// Index used for `Screen::ProtocolDetail { protocol_idx }`.
        protocol_idx: usize,
    }

    let protocols = [
        ProtocolItem {
            name: "Mirage",
            desc: "Нативный протокол с маскировкой трафика",
            toggle_id: "proto_mirage",
            protocol_idx: 0,
        },
        ProtocolItem {
            name: "VLESS+Reality",
            desc: "XRay VLESS с Reality TLS-маскировкой",
            toggle_id: "proto_vless",
            protocol_idx: 1,
        },
        ProtocolItem {
            name: "XHTTP",
            desc: "XRay транспорт XHTTP",
            toggle_id: "proto_xhttp",
            protocol_idx: 2,
        },
        ProtocolItem {
            name: "ShadowTLS",
            desc: "ShadowTLS — скрытие трафика под TLS",
            toggle_id: "proto_shadowtls",
            protocol_idx: 3,
        },
        ProtocolItem {
            name: "Hysteria2",
            desc: "Протокол Hysteria2 на основе QUIC",
            toggle_id: "proto_hysteria2",
            protocol_idx: 4,
        },
    ];

    let item_h = 72.0;
    let list_h = item_h * protocols.len() as f64;

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, w, list_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, w, list_h, 16.0);

    for (i, proto) in protocols.iter().enumerate() {
        let item_y = y + i as f64 * item_h;

        // Row hover detection (excluding toggle area)
        let row_rect = Rect::new(margin, item_y, w - 70.0, item_h);
        let row_id = WidgetId::new(format!("proto_row_{i}"));
        state.coordinator.register(row_id.clone(), row_rect, Sense::CLICK);
        let is_hovered = state.coordinator.is_hovered(&row_id);

        if is_hovered {
            ctx.set_fill_color_alpha(SURFACE_2, 0.5);
            ctx.fill_rect(margin, item_y, w, item_h);
        }

        // Protocol name
        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("bold 15px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(proto.name, margin + 16.0, item_y + item_h / 2.0 - 2.0);

        // Description
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("12px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(proto.desc, margin + 16.0, item_y + item_h / 2.0 + 4.0);

        // Toggle on the right
        let toggle_x = margin + w - 16.0 - 44.0;
        let toggle_y = item_y + (item_h - 26.0) / 2.0;
        draw_toggle(ctx, input, state, proto.toggle_id, toggle_x, toggle_y);

        // Chevron (slightly to the right of center for the row area)
        let chevron_size = 18.0;
        draw_svg_icon(
            ctx,
            ICON_CHEVRON_RIGHT,
            margin + w - 44.0 - chevron_size - 10.0,
            item_y + (item_h - chevron_size) / 2.0,
            chevron_size,
            chevron_size,
            TEXT_MUTED,
        );

        if i < protocols.len().saturating_sub(1) {
            draw_divider(ctx, margin + 16.0, item_y + item_h, w - 16.0);
        }

        // Navigate to ProtocolDetail on row click
        if clicked && is_hovered {
            let pidx = proto.protocol_idx;
            state.push_screen(Screen::ProtocolDetail { chain_idx, protocol_idx: pidx });
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Tab 1: Сервисы
// ---------------------------------------------------------------------------

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
    // --- DNS row ---
    let dns_primary = state.dns_primary.clone();
    let dns_secondary = state.dns_secondary.clone();
    let dns_value = format!("{} / {}", dns_primary, dns_secondary);

    let dns_h = 64.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, w, dns_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, w, dns_h, 12.0);

    let dns_row_id = WidgetId::new("svc_dns_row");
    let dns_rect = Rect::new(margin, y, w, dns_h);
    state.coordinator.register(dns_row_id.clone(), dns_rect, Sense::CLICK);
    let dns_hovered = state.coordinator.is_hovered(&dns_row_id);
    if dns_hovered {
        ctx.set_fill_color_alpha(SURFACE_2, 0.5);
        ctx.fill_rect(margin, y, w, dns_h);
    }

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 15px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Bottom);
    ctx.fill_text("DNS", margin + 16.0, y + dns_h / 2.0 - 2.0);

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("12px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(&dns_value, margin + 16.0, y + dns_h / 2.0 + 4.0);

    let chevron_size = 18.0;
    draw_svg_icon(
        ctx,
        ICON_CHEVRON_RIGHT,
        margin + w - chevron_size - 14.0,
        y + (dns_h - chevron_size) / 2.0,
        chevron_size,
        chevron_size,
        TEXT_MUTED,
    );

    let _ = clicked && dns_hovered;

    // --- Split tunneling row ---
    let st_y = y + dns_h + 12.0;
    let st_h = 64.0;

    let split_enabled = state.split_tunneling_enabled;
    let split_label = if split_enabled { "Включено" } else { "Выключено" };

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, st_y, w, st_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, st_y, w, st_h, 12.0);

    let st_row_id = WidgetId::new("svc_split_row");
    let st_rect = Rect::new(margin, st_y, w, st_h);
    state.coordinator.register(st_row_id.clone(), st_rect, Sense::CLICK);
    let st_hovered = state.coordinator.is_hovered(&st_row_id);
    if st_hovered {
        ctx.set_fill_color_alpha(SURFACE_2, 0.5);
        ctx.fill_rect(margin, st_y, w, st_h);
    }

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 15px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Bottom);
    ctx.fill_text("Раздельное туннелирование", margin + 16.0, st_y + st_h / 2.0 - 2.0);

    let status_color = if split_enabled { ACCENT } else { TEXT_MUTED };
    ctx.set_fill_color(status_color);
    ctx.set_font("12px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(split_label, margin + 16.0, st_y + st_h / 2.0 + 4.0);

    draw_svg_icon(
        ctx,
        ICON_CHEVRON_RIGHT,
        margin + w - chevron_size - 14.0,
        st_y + (st_h - chevron_size) / 2.0,
        chevron_size,
        chevron_size,
        TEXT_MUTED,
    );

    let _ = clicked && st_hovered;
}

// ---------------------------------------------------------------------------
// Tab 2: Управление
// ---------------------------------------------------------------------------

fn draw_management_tab(
    ctx: &mut dyn RenderContext,
    _input: &InputState,
    state: &mut AppState,
    margin: f64,
    y: f64,
    w: f64,
    _h: f64,
    clicked: bool,
    chain_idx: usize,
) {
    struct MgmtButton {
        label: &'static str,
        id: &'static str,
        /// "accent" | "warning" | "error"
        style: &'static str,
    }

    let buttons = [
        MgmtButton { label: "Проверить сервер",   id: "mgmt_check",  style: "accent"  },
        MgmtButton { label: "Перезагрузить",       id: "mgmt_reboot", style: "warning" },
        MgmtButton { label: "Удалить сервер",      id: "mgmt_remove", style: "error"   },
        MgmtButton { label: "Очистить данные",     id: "mgmt_clean",  style: "error"   },
    ];

    let btn_h = 56.0;
    let gap = 10.0;

    for (i, btn) in buttons.iter().enumerate() {
        let btn_y = y + i as f64 * (btn_h + gap);
        let rect = Rect::new(margin, btn_y, w, btn_h);
        let widget_id = WidgetId::new(btn.id);
        state.coordinator.register(widget_id.clone(), rect, Sense::CLICK);
        let is_hovered = state.coordinator.is_hovered(&widget_id);

        let (bg_color, border_color, text_color): (&str, &str, &str) = match btn.style {
            "accent" => {
                if is_hovered { ("#2A2010", ACCENT, ACCENT) } else { (SURFACE, SURFACE_2, ACCENT) }
            }
            "warning" => {
                if is_hovered { ("#2A2000", "#E6A817", "#E6A817") } else { (SURFACE, SURFACE_2, "#E6A817") }
            }
            _ => {
                // error / red
                if is_hovered { ("#3A1A1A", ERROR_COLOR, ERROR_COLOR) } else { (SURFACE, SURFACE_2, ERROR_COLOR) }
            }
        };

        ctx.set_fill_color(bg_color);
        ctx.fill_rounded_rect(margin, btn_y, w, btn_h, 12.0);
        ctx.set_stroke_color(border_color);
        ctx.set_stroke_width(1.0);
        ctx.stroke_rounded_rect(margin, btn_y, w, btn_h, 12.0);

        ctx.set_fill_color(text_color);
        ctx.set_font("bold 15px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(btn.label, margin + w / 2.0, btn_y + btn_h / 2.0);

        if clicked && is_hovered {
            match btn.id {
                "mgmt_check" => {
                    state.push_log("Проверка сервера...");
                }
                "mgmt_reboot" => {
                    state.show_confirm(ConfirmDrawer {
                        title: "Перезагрузить сервер?".into(),
                        description: "Сервер будет недоступен около 30 секунд.".into(),
                        confirm_label: "Перезагрузить".into(),
                        cancel_label: "Отмена".into(),
                        action_id: format!("reboot_chain_{chain_idx}"),
                    });
                }
                "mgmt_remove" => {
                    state.show_confirm(ConfirmDrawer {
                        title: "Удалить сервер?".into(),
                        description: "Сервер будет удалён из приложения.".into(),
                        confirm_label: "Удалить".into(),
                        cancel_label: "Отмена".into(),
                        action_id: format!("remove_chain_{chain_idx}"),
                    });
                }
                "mgmt_clean" => {
                    state.show_confirm(ConfirmDrawer {
                        title: "Очистить данные?".into(),
                        description: "Все данные цепи будут сброшены.".into(),
                        confirm_label: "Очистить".into(),
                        cancel_label: "Отмена".into(),
                        action_id: format!("clear_chain_{chain_idx}"),
                    });
                }
                _ => {}
            }
            return;
        }
    }
}
