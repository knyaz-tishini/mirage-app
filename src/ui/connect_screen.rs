//! Quick Connect screen — paste key, add config file, or scan QR code.

use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::{Rect, WidgetId};
use uzor::input::{InputState, Sense};

use crate::app::AppState;
use super::components::{
    BG, ACCENT, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED, SEPARATOR,
    ICON_CHEVRON_RIGHT,
    draw_nav_bar,
};
use super::main_screen::navigate_tab;

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
    ctx.set_font("bold 18px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Соединение", content_x, 24.0);

    // Subtitle
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px sans-serif");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(
        "Вставьте ключ, добавьте файл конфигурации",
        content_x,
        54.0,
    );
    ctx.fill_text("или отсканируйте QR-код", content_x, 72.0);

    // -------------------------------------------------------------------------
    // Paste key input area
    // -------------------------------------------------------------------------
    let input_y = 108.0;
    let input_h = 52.0;
    let btn_w = 90.0;
    let btn_pad = 8.0;

    // Input background
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(content_x, input_y, content_w, input_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(content_x, input_y, content_w, input_h, 12.0);

    // Placeholder text (left side)
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Вставьте ключ", content_x + 16.0, input_y + input_h / 2.0);

    // Orange "Вставить" button (right side)
    let btn_x = content_x + content_w - btn_w - btn_pad;
    let btn_y = input_y + (input_h - 36.0) / 2.0;
    let btn_rect = Rect::new(btn_x, btn_y, btn_w, 36.0);
    let paste_id = WidgetId::new("connect_paste");
    state.coordinator.register(paste_id.clone(), btn_rect, Sense::CLICK);
    let btn_hovered = state.coordinator.is_hovered(&paste_id);

    let btn_color = if btn_hovered { "#f5a550" } else { ACCENT };
    ctx.set_fill_color(btn_color);
    ctx.fill_rounded_rect(btn_rect.x, btn_rect.y, btn_rect.width, btn_rect.height, 8.0);

    ctx.set_fill_color("#0E0E11");
    ctx.set_font("bold 13px sans-serif");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Вставить", btn_x + btn_w / 2.0, btn_y + 18.0);

    // -------------------------------------------------------------------------
    // "Другие варианты подключения" section
    // -------------------------------------------------------------------------
    let section_y = input_y + input_h + 28.0;

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("ДРУГИЕ ВАРИАНТЫ ПОДКЛЮЧЕНИЯ", content_x, section_y);

    // -------------------------------------------------------------------------
    // Option cards
    // -------------------------------------------------------------------------
    struct OptionCard {
        title: &'static str,
        subtitle: &'static str,
        id: &'static str,
    }

    let options = [
        OptionCard {
            title: "VPN от Amnezia",
            subtitle: "Подключайтесь к классическим платным и бесплатным VPN-сервисам от Amnezia",
            id: "connect_amnezia",
        },
        OptionCard {
            title: "Self-hosted VPN",
            subtitle: "Настроить VPN на собственном сервере",
            id: "connect_selfhosted",
        },
    ];

    let card_h = 72.0;
    let cards_top = section_y + 22.0;
    let cards_total_h = card_h * options.len() as f64;

    // Shared card background
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(content_x, cards_top, content_w, cards_total_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(content_x, cards_top, content_w, cards_total_h, 12.0);

    let clicked = input.is_clicked();

    for (i, option) in options.iter().enumerate() {
        let card_y = cards_top + i as f64 * card_h;
        let card_rect = Rect::new(content_x, card_y, content_w, card_h);
        let card_id = WidgetId::new(option.id);

        state.coordinator.register(card_id.clone(), card_rect, Sense::CLICK);
        let card_hovered = state.coordinator.is_hovered(&card_id);

        if card_hovered {
            ctx.set_fill_color_alpha(SURFACE_2, 0.5);
            if i == 0 {
                // Rounded top only
                ctx.fill_rounded_rect(content_x, card_y, content_w, card_h, 12.0);
                ctx.fill_rect(content_x, card_y + 12.0, content_w, card_h - 12.0);
            } else {
                // Rounded bottom only
                ctx.fill_rounded_rect(content_x, card_y, content_w, card_h, 12.0);
                ctx.fill_rect(content_x, card_y, content_w, card_h - 12.0);
            }
        }

        let text_pad = 16.0;

        // Card title
        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("bold 15px sans-serif");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(option.title, content_x + text_pad, card_y + 14.0);

        // Card subtitle (muted, wrap-truncated)
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("12px sans-serif");
        ctx.set_text_baseline(TextBaseline::Top);
        let subtitle = truncate_text(option.subtitle, 52);
        ctx.fill_text(&subtitle, content_x + text_pad, card_y + 36.0);

        // Chevron right
        let chevron_size = 18.0;
        draw_svg_icon(
            ctx,
            ICON_CHEVRON_RIGHT,
            content_x + content_w - chevron_size - text_pad,
            card_y + (card_h - chevron_size) / 2.0,
            chevron_size,
            chevron_size,
            TEXT_MUTED,
        );

        // Divider between cards (not after last)
        if i < options.len().saturating_sub(1) {
            ctx.set_stroke_color(SEPARATOR);
            ctx.set_stroke_width(0.5);
            ctx.begin_path();
            ctx.move_to(content_x + text_pad, card_y + card_h);
            ctx.line_to(content_x + content_w - text_pad, card_y + card_h);
            ctx.stroke();
        }

        // Click handling (no-op for now — visual only)
        let _ = clicked && card_hovered;
    }

    // Nav bar
    let active_tab = 3;
    if let Some(tab) = draw_nav_bar(ctx, input, state, w, h, active_tab) {
        navigate_tab(state, tab);
    }
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}
