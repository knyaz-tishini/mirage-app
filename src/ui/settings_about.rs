//! Settings → О приложении screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::Rect;

use crate::app::AppState;
use super::components::{
    BG, SURFACE, SURFACE_2, ACCENT, TEXT_PRIMARY, TEXT_MUTED,
    ICON_SERVER,
    draw_back_header, draw_divider,
};

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

    if draw_back_header(ctx, input, state, "О приложении", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let content_w = w - margin * 2.0;

    // Logo placeholder (circle)
    let logo_cx = w / 2.0;
    let logo_cy = 130.0;
    let logo_r = 48.0;
    ctx.begin_path();
    ctx.arc(logo_cx, logo_cy, logo_r, 0.0, std::f64::consts::PI * 2.0);
    ctx.set_fill_color(SURFACE);
    ctx.fill();
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke();

    // Title
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 22px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Mirage VPN", w / 2.0, logo_cy + logo_r + 16.0);

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(
        "Безопасное и открытое приложение для VPN",
        w / 2.0,
        logo_cy + logo_r + 44.0,
    );

    // Contacts section
    let contacts_y = logo_cy + logo_r + 80.0;
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.fill_text("Контакты", margin, contacts_y);

    // Contact items
    struct Contact {
        title: &'static str,
        desc: &'static str,
    }
    let contacts = [
        Contact { title: "Telegram",                   desc: "Обсудить функции"     },
        Contact { title: "support@mirage-vpn.org",     desc: "Отзывы и баг-репорты" },
        Contact { title: "GitHub",                     desc: "Исходный код"          },
        Contact { title: "Сайт",                       desc: "Официальный сайт"      },
    ];

    let item_h = 56.0;
    let list_top = contacts_y + 22.0;
    let list_h = item_h * contacts.len() as f64;

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, list_top, content_w, list_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, list_top, content_w, list_h, 16.0);

    for (i, contact) in contacts.iter().enumerate() {
        let item_y = list_top + i as f64 * item_h;
        let icon_size = 22.0;

        ctx.set_fill_color(TEXT_MUTED);
        ctx.fill_rect(margin + 14.0, item_y + (item_h - icon_size) / 2.0, icon_size, icon_size);

        // Use ICON_SERVER as placeholder for all contact icons
        let _ = ICON_SERVER;

        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("16px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(contact.title, margin + 52.0, item_y + item_h / 2.0 - 1.0);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(contact.desc, margin + 52.0, item_y + item_h / 2.0 + 3.0);

        if i < contacts.len().saturating_sub(1) {
            draw_divider(ctx, margin + 52.0, item_y + item_h, content_w - 52.0);
        }
    }

    // Version footer
    let footer_y = list_top + list_h + 24.0;
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Версия: 0.1.3", w / 2.0, footer_y);

    // "Проверить обновления" link
    let link_y = footer_y + 28.0;
    ctx.set_fill_color(ACCENT);
    ctx.set_font("14px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Проверить обновления", w / 2.0, link_y);

    // SOCKS5 info
    let socks5_y = link_y + 28.0;
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.fill_text(&format!("SOCKS5: {}", state.socks5_addr), w / 2.0, socks5_y);

    // Config path
    let path_y = socks5_y + 22.0;
    let display_path = truncate_path(&state.config_path, 40);
    ctx.set_font("11px pt root ui");
    ctx.fill_text(&display_path, w / 2.0, path_y);

    let _ = h;
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
