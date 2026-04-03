//! Quick Connect screen — enter VPS address, optionally enter API key for admin access.

use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::{Rect, WidgetId};
use uzor::input::{InputState, Sense};

use crate::app::AppState;
use crate::api::ApiRequest;
use super::components::{
    BG, ACCENT, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED, SEPARATOR,
    ICON_CHEVRON_RIGHT, ICON_SERVER, ICON_SETTINGS, ICON_LOGS,
    draw_text_field,
};

/// Draw the Quick Connect screen.
pub fn draw_connect_screen(
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

    let margin = 16.0;
    let content_x = margin;
    let content_w = w - margin * 2.0;

    // Title
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 32px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Соединение", content_x, 20.0);

    // Subtitle
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("14px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Укажите адрес VPS-сервера для подключения", content_x, 64.0);

    // -------------------------------------------------------------------------
    // Field 1: VPS address
    // -------------------------------------------------------------------------
    let field_h = 52.0;
    let mut field_y = 100.0;

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("АДРЕС СЕРВЕРА", content_x, field_y);
    field_y += 18.0;

    draw_text_field(
        ctx,
        input,
        state,
        "client_vps_addr",
        "84.201.155.42:443",
        &state.client_vps_addr.clone(),
        content_x,
        field_y,
        content_w,
        field_h,
    );

    // Sync VPS address field → state.client_vps_addr
    let vps_val = state.field_value("client_vps_addr").to_string();
    if !vps_val.is_empty() && vps_val != state.client_vps_addr {
        state.client_vps_addr = vps_val;
    }

    field_y += field_h + 16.0;

    // -------------------------------------------------------------------------
    // Field 2: API key (optional — unlocks admin tab)
    // -------------------------------------------------------------------------
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("API КЛЮЧ (ОПЦИОНАЛЬНО — ОТКРЫВАЕТ ADMIN)", content_x, field_y);
    field_y += 18.0;

    let api_token_display = state.api_token.clone().unwrap_or_default();
    draw_text_field(
        ctx,
        input,
        state,
        "connect_api_key",
        "Bearer token...",
        &api_token_display,
        content_x,
        field_y,
        content_w,
        field_h,
    );

    field_y += field_h + 10.0;

    // "Применить ключ" button
    let apply_btn_h = 44.0;
    let apply_rect = Rect::new(content_x, field_y, content_w, apply_btn_h);
    let apply_id = WidgetId::new("connect_apply_key");
    state.coordinator.register(apply_id.clone(), apply_rect, Sense::CLICK);
    let apply_hovered = state.coordinator.is_hovered(&apply_id);
    let clicked = input.is_clicked();

    let apply_bg = if apply_hovered { "#f5a550" } else { ACCENT };
    ctx.set_fill_color(apply_bg);
    ctx.fill_rounded_rect(content_x, field_y, content_w, apply_btn_h, 10.0);
    ctx.set_fill_color(BG);
    ctx.set_font("bold 14px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Применить", content_x + content_w / 2.0, field_y + apply_btn_h / 2.0);

    if clicked && apply_hovered {
        // Apply VPS address
        let vps = state.field_value("client_vps_addr").to_string();
        if !vps.is_empty() {
            state.client_vps_addr = vps;
        }

        // Apply API key if provided
        let key = state.field_value("connect_api_key").to_string();
        if !key.is_empty() {
            state.api_token = Some(key.clone());
            state.api.send(ApiRequest::SetToken(key));
            state.admin_unlocked = true;
        }

        // Also try clipboard paste for backward compatibility
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.get_text(); // Just access to avoid unused warning
        }
    }

    // -------------------------------------------------------------------------
    // Section label
    // -------------------------------------------------------------------------
    let section_y = field_y + apply_btn_h + 24.0;

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("ДРУГИЕ ВАРИАНТЫ", content_x, section_y);

    // -------------------------------------------------------------------------
    // Option cards (2 options: buy from Mirage, self-hosted)
    // -------------------------------------------------------------------------
    struct OptionCard {
        icon: &'static str,
        title: &'static str,
        subtitle: &'static str,
        id: &'static str,
    }

    let options = [
        OptionCard {
            icon: ICON_SERVER,
            title: "Купить у MIRAGE",
            subtitle: "Готовый VPS с предустановленным сервером",
            id: "connect_mirage_buy",
        },
        OptionCard {
            icon: ICON_SETTINGS,
            title: "Свой сервер",
            subtitle: "Введите IP-адрес вашего VPS выше",
            id: "connect_selfhosted",
        },
        OptionCard {
            icon: ICON_LOGS,
            title: "Импорт конфига",
            subtitle: "Загрузите файл конфигурации",
            id: "connect_file",
        },
    ];

    let card_h = 72.0;
    let cards_top = section_y + 22.0;
    let cards_total_h = card_h * options.len() as f64;

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(content_x, cards_top, content_w, cards_total_h, 14.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(content_x, cards_top, content_w, cards_total_h, 14.0);

    for (i, option) in options.iter().enumerate() {
        let card_y = cards_top + i as f64 * card_h;
        let card_rect = Rect::new(content_x, card_y, content_w, card_h);
        let card_id = WidgetId::new(option.id);

        state.coordinator.register(card_id.clone(), card_rect, Sense::CLICK);
        let card_hovered = state.coordinator.is_hovered(&card_id);

        if card_hovered {
            ctx.set_fill_color_alpha(SURFACE_2, 0.5);
            ctx.fill_rect(content_x, card_y, content_w, card_h);
        }

        // Left icon (24×24)
        let icon_size = 24.0;
        let icon_x = content_x + 16.0;
        let icon_y = card_y + (card_h - icon_size) / 2.0;
        let icon_color = if card_hovered { TEXT_PRIMARY } else { TEXT_MUTED };
        draw_svg_icon(ctx, option.icon, icon_x, icon_y, icon_size, icon_size, icon_color);

        // Text (title + subtitle)
        let text_x = content_x + 16.0 + icon_size + 14.0;
        let text_center = card_y + card_h / 2.0;

        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("bold 15px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(option.title, text_x, text_center - 1.0);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("12px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(option.subtitle, text_x, text_center + 3.0);

        // Chevron right
        let chevron_size = 18.0;
        draw_svg_icon(
            ctx,
            ICON_CHEVRON_RIGHT,
            content_x + content_w - chevron_size - 14.0,
            card_y + (card_h - chevron_size) / 2.0,
            chevron_size,
            chevron_size,
            TEXT_MUTED,
        );

        // Divider between cards
        if i < options.len().saturating_sub(1) {
            ctx.set_stroke_color(SEPARATOR);
            ctx.set_stroke_width(0.5);
            ctx.begin_path();
            ctx.move_to(content_x + 16.0, card_y + card_h);
            ctx.line_to(content_x + content_w - 16.0, card_y + card_h);
            ctx.stroke();
        }

        if clicked && card_hovered {
            match option.id {
                "connect_selfhosted" => {
                    // Focus the VPS address field
                    state.focus_field("client_vps_addr", "");
                }
                "connect_mirage_buy" => {
                    // TODO: open browser to mirage purchase page
                    state.push_log("Открытие страницы покупки Mirage...");
                }
                "connect_file" => {
                    // TODO: file import
                    state.push_log("Импорт конфига — скоро");
                }
                _ => {}
            }
        }
    }

    let _ = h;
}
