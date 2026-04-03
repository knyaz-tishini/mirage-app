//! Quick Connect screen — paste key, add config file, or scan QR code.

use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::{Rect, WidgetId};
use uzor::input::{InputState, Sense};

use crate::app::AppState;
use super::components::{
    BG, ACCENT, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED, SEPARATOR,
    ICON_CHEVRON_RIGHT, ICON_SERVER, ICON_SETTINGS, ICON_LOGS,
    draw_nav_bar, draw_text_field,
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
    ctx.fill_text("Вставьте ключ подключения или выберите способ", content_x, 64.0);

    // -------------------------------------------------------------------------
    // Text field for connection key
    // -------------------------------------------------------------------------
    let field_y = 102.0;
    let field_h = 52.0;
    draw_text_field(
        ctx,
        input,
        state,
        "connect_key",
        "amvpn://...",
        "",
        content_x,
        field_y,
        content_w,
        field_h,
    );

    // -------------------------------------------------------------------------
    // "Вставить из буфера" button — reads clipboard via arboard
    // -------------------------------------------------------------------------
    let paste_btn_y = field_y + field_h + 10.0;
    let paste_btn_h = 44.0;
    let paste_rect = Rect::new(content_x, paste_btn_y, content_w, paste_btn_h);
    let paste_id = WidgetId::new("connect_paste_clipboard");
    state.coordinator.register(paste_id.clone(), paste_rect, Sense::CLICK);
    let paste_hovered = state.coordinator.is_hovered(&paste_id);
    let clicked = input.is_clicked();

    let paste_bg = if paste_hovered { "#f5a550" } else { ACCENT };
    ctx.set_fill_color(paste_bg);
    ctx.fill_rounded_rect(content_x, paste_btn_y, content_w, paste_btn_h, 10.0);
    ctx.set_fill_color(BG);
    ctx.set_font("bold 14px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Вставить из буфера", content_x + content_w / 2.0, paste_btn_y + paste_btn_h / 2.0);

    if clicked && paste_hovered {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if let Ok(text) = cb.get_text() {
                state.field_set_text("connect_key", text);
            }
        }
    }

    // -------------------------------------------------------------------------
    // Section label
    // -------------------------------------------------------------------------
    let section_y = paste_btn_y + paste_btn_h + 24.0;

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("ДРУГИЕ ВАРИАНТЫ ПОДКЛЮЧЕНИЯ", content_x, section_y);

    // -------------------------------------------------------------------------
    // Option cards
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
            title: "VPN от Mirage",
            subtitle: "Получите бесплатный VPN-сервер",
            id: "connect_mirage",
        },
        OptionCard {
            icon: ICON_SETTINGS,
            title: "Self-hosted VPN",
            subtitle: "Настройте свой VPN на сервере",
            id: "connect_selfhosted",
        },
        OptionCard {
            icon: ICON_LOGS,
            title: "Импорт из файла",
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

        let _ = clicked && card_hovered;
    }

    // Nav bar
    if let Some(tab) = draw_nav_bar(ctx, input, state, w, h, 3) {
        state.go_to_tab(tab);
    }
}
