//! Settings → Соединение screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::Rect;

use crate::app::{AppState, Screen};
use super::components::{
    BG, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED,
    ICON_SETTINGS, ICON_LOGS, ICON_CHEVRON_RIGHT,
    draw_back_header, draw_divider, draw_toggle,
};
use uzor::render::draw_svg_icon;
use uzor::types::WidgetId;
use uzor::input::Sense;

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

    if draw_back_header(ctx, input, state, "Соединение", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let content_w = w - margin * 2.0;
    let mut y = 80.0;
    let row_h = 60.0;
    let clicked = input.is_clicked();

    // ── Section: DNS, Split tunneling, KillSwitch ──────────────────────────
    struct NavRow {
        icon: &'static str,
        title: &'static str,
        subtitle: &'static str,
        id: &'static str,
        toggle_id: Option<&'static str>,
        screen: Screen,
    }

    let rows: &[NavRow] = &[
        NavRow {
            icon: ICON_LOGS,
            title: "DNS",
            subtitle: "Настройка DNS-серверов",
            id: "conn_row_dns",
            toggle_id: Some("dns_enabled"),
            screen: Screen::SettingsDns,
        },
        NavRow {
            icon: ICON_SETTINGS,
            title: "Раздельное туннелирование",
            subtitle: "Выборочный маршрут трафика",
            id: "conn_row_split",
            toggle_id: None,
            screen: Screen::SettingsSplitTunneling,
        },
        NavRow {
            icon: ICON_SETTINGS,
            title: "KillSwitch",
            subtitle: "Блокировка при разрыве VPN",
            id: "conn_row_ks",
            toggle_id: Some("killswitch_enabled"),
            screen: Screen::SettingsKillSwitch,
        },
    ];

    let list_h = row_h * rows.len() as f64;

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, list_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, list_h, 16.0);

    let mut navigate_to: Option<Screen> = None;

    for (i, row) in rows.iter().enumerate() {
        let row_y = y + i as f64 * row_h;
        let icon_size = 24.0;
        let icon_margin = 16.0;
        let icon_cy = row_y + row_h / 2.0;

        // Row hit area (excludes toggle area so toggle gets priority)
        let row_rect = Rect::new(margin, row_y, content_w, row_h);
        let row_id = WidgetId::new(row.id);
        state.coordinator.register(row_id.clone(), row_rect, Sense::CLICK);
        let row_hovered = state.coordinator.is_hovered(&row_id);

        if row_hovered {
            ctx.set_fill_color_alpha(SURFACE_2, 0.4);
            ctx.fill_rect(margin, row_y, content_w, row_h);
        }

        // Left icon
        draw_svg_icon(
            ctx,
            row.icon,
            margin + icon_margin,
            icon_cy - icon_size / 2.0,
            icon_size,
            icon_size,
            TEXT_MUTED,
        );

        // Title + subtitle
        let text_x = margin + icon_margin + icon_size + 14.0;
        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("15px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(row.title, text_x, icon_cy - 1.0);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("12px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(row.subtitle, text_x, icon_cy + 3.0);

        // Right side: toggle (if any) + chevron
        let chevron_size = 20.0;
        let right_x = margin + content_w - chevron_size - 14.0;

        if let Some(toggle_id) = row.toggle_id {
            let pill_w = 44.0;
            let pill_h = 26.0;
            let pill_x = right_x - chevron_size - pill_w - 8.0;
            let pill_y = row_y + (row_h - pill_h) / 2.0;
            draw_toggle(ctx, input, state, toggle_id, pill_x, pill_y);
        }

        draw_svg_icon(
            ctx,
            ICON_CHEVRON_RIGHT,
            right_x,
            icon_cy - chevron_size / 2.0,
            chevron_size,
            chevron_size,
            TEXT_MUTED,
        );

        if i < rows.len().saturating_sub(1) {
            draw_divider(ctx, margin + 52.0, row_y + row_h, content_w - 52.0);
        }

        if clicked && row_hovered {
            navigate_to = Some(row.screen.clone());
        }
    }

    y += list_h + 20.0;

    // ── Description text ───────────────────────────────────────────────────
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("12px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Настройте параметры безопасного соединения.", margin, y);

    if let Some(screen) = navigate_to {
        state.push_screen(screen);
    }

    let _ = h;
}
