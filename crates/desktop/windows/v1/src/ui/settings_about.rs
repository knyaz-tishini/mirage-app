//! Settings → О приложении screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::{Rect, WidgetId};
use uzor::input::Sense;

use crate::app::AppState;
use super::components::{
    BG, SURFACE, SURFACE_2, ACCENT, TEXT_PRIMARY, TEXT_MUTED,
    ICON_CHEVRON_RIGHT,
    draw_back_header, draw_divider,
};
use uzor::render::draw_svg_icon;

struct LinkRow {
    title: &'static str,
    subtitle: &'static str,
    url: &'static str,
    id: &'static str,
}

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
    let clicked = input.is_clicked();

    // ── App logo area ──────────────────────────────────────────────────────
    let logo_cx = w / 2.0;
    let logo_cy = 130.0;
    let logo_r = 48.0;

    // Outer glow ring
    ctx.set_fill_color_alpha(ACCENT, 0.12);
    ctx.begin_path();
    ctx.arc(logo_cx, logo_cy, logo_r + 8.0, 0.0, std::f64::consts::PI * 2.0);
    ctx.fill();

    // Logo circle
    ctx.set_fill_color(SURFACE);
    ctx.begin_path();
    ctx.arc(logo_cx, logo_cy, logo_r, 0.0, std::f64::consts::PI * 2.0);
    ctx.fill();
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.5);
    ctx.stroke();

    // "M" letter
    ctx.set_fill_color(ACCENT);
    ctx.set_font("bold 36px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("M", logo_cx, logo_cy);

    // App name
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 22px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Mirage VPN", logo_cx, logo_cy + logo_r + 16.0);

    // Version
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.fill_text("v0.1.4", logo_cx, logo_cy + logo_r + 44.0);

    // Divider
    let divider_y = logo_cy + logo_r + 72.0;
    draw_divider(ctx, margin, divider_y, content_w);

    // ── Link rows ──────────────────────────────────────────────────────────
    let links: &[LinkRow] = &[
        LinkRow {
            title: "Telegram",
            subtitle: "Чат и поддержка",
            url: "https://t.me/mirage_vpn",
            id: "about_tg",
        },
        LinkRow {
            title: "GitHub",
            subtitle: "Исходный код проекта",
            url: "https://github.com/mirage-vpn/mirage",
            id: "about_gh",
        },
        LinkRow {
            title: "Веб-сайт",
            subtitle: "Официальный сайт",
            url: "https://mirage-vpn.org",
            id: "about_web",
        },
        LinkRow {
            title: "Политика конфиденциальности",
            subtitle: "Privacy policy",
            url: "https://mirage-vpn.org/privacy",
            id: "about_privacy",
        },
    ];

    let row_h = 56.0;
    let list_top = divider_y + 16.0;
    let list_h = row_h * links.len() as f64;

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, list_top, content_w, list_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, list_top, content_w, list_h, 16.0);

    let mut open_url: Option<&'static str> = None;

    for (i, link) in links.iter().enumerate() {
        let row_y = list_top + i as f64 * row_h;
        let mid_y = row_y + row_h / 2.0;

        let row_id = WidgetId::new(link.id);
        let row_rect = Rect::new(margin, row_y, content_w, row_h);
        state.coordinator.register(row_id.clone(), row_rect, Sense::CLICK);
        let row_hovered = state.coordinator.is_hovered(&row_id);

        if row_hovered {
            ctx.set_fill_color_alpha(SURFACE_2, 0.4);
            ctx.fill_rect(margin, row_y, content_w, row_h);
        }

        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("15px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(link.title, margin + 16.0, mid_y - 1.0);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("12px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(link.subtitle, margin + 16.0, mid_y + 3.0);

        let chev_size = 20.0;
        draw_svg_icon(
            ctx,
            ICON_CHEVRON_RIGHT,
            margin + content_w - chev_size - 14.0,
            mid_y - chev_size / 2.0,
            chev_size,
            chev_size,
            TEXT_MUTED,
        );

        if i < links.len().saturating_sub(1) {
            draw_divider(ctx, margin + 16.0, row_y + row_h, content_w - 16.0);
        }

        if clicked && row_hovered {
            open_url = Some(link.url);
        }
    }

    if let Some(url) = open_url {
        let _ = open::that(url);
    }

    // ── Version footer ─────────────────────────────────────────────────────
    let footer_y = list_top + list_h + 24.0;
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(
        &format!("SOCKS5: {}", state.socks5_addr),
        w / 2.0,
        footer_y,
    );

    let display_path = truncate_path(&state.config_path, 40);
    ctx.set_font("10px pt root ui");
    ctx.fill_text(&display_path, w / 2.0, footer_y + 18.0);

    let _ = h;
    let _ = ACCENT;
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
