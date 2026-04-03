//! Settings → Соединение screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::{Rect, WidgetId};
use uzor::input::Sense;

use crate::app::{AppState, Screen};
use super::components::{
    BG, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED,
    ICON_SETTINGS, ICON_LOGS,
    draw_back_header, draw_list_item, draw_divider,
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

    if draw_back_header(ctx, input, state, "Соединение", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let content_w = w - margin * 2.0;
    let mut y = 80.0;

    // Toggle: Use AmneziaDNS
    let toggle_h = 56.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, toggle_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, toggle_h, 12.0);

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("16px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Bottom);
    ctx.fill_text("Использовать AmneziaDNS", margin + 16.0, y + toggle_h / 2.0 - 1.0);

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Если AmneziaDNS установлен на сервере", margin + 16.0, y + toggle_h / 2.0 + 3.0);

    // Toggle pill (placeholder off)
    let pill_w = 44.0;
    let pill_h = 26.0;
    let pill_x = margin + content_w - pill_w - 16.0;
    let pill_y = y + (toggle_h - pill_h) / 2.0;
    ctx.set_fill_color(SURFACE_2);
    ctx.fill_rounded_rect(pill_x, pill_y, pill_w, pill_h, 13.0);

    y += toggle_h + 12.0;

    // List items
    struct ConnItem {
        icon: &'static str,
        title: &'static str,
        subtitle: &'static str,
        id: &'static str,
        screen: Screen,
    }

    let items: &[ConnItem] = &[
        ConnItem {
            icon: ICON_LOGS,
            title: "DNS-серверы",
            subtitle: "Когда AmneziaDNS не используется",
            id: "conn_dns",
            screen: Screen::SettingsDns,
        },
        ConnItem {
            icon: ICON_SETTINGS,
            title: "Сайт-based split tunneling",
            subtitle: "Раздельное туннелирование по сайтам",
            id: "conn_split",
            screen: Screen::SettingsSplitTunneling,
        },
    ];

    let item_h = 68.0;
    let list_h = item_h * items.len() as f64;

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, list_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, list_h, 16.0);

    let mut navigate_to: Option<Screen> = None;

    for (i, item) in items.iter().enumerate() {
        let item_y = y + i as f64 * item_h;

        let clicked = draw_list_item(
            ctx,
            input,
            state,
            item.id,
            item.icon,
            item.title,
            item.subtitle,
            margin,
            item_y,
            content_w,
            item_h,
        );

        if i < items.len().saturating_sub(1) {
            draw_divider(ctx, margin + 52.0, item_y + item_h, content_w - 52.0);
        }

        if clicked {
            navigate_to = Some(item.screen.clone());
        }
    }

    if let Some(screen) = navigate_to {
        state.push_screen(screen);
    }

    // Suppress unused
    let _ = h;
    let _ = WidgetId::new("x");
    let _ = Sense::CLICK;
}
