//! Settings screen — Amnezia-style settings list, Russian text.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;

use crate::app::{AppState, Screen};
use super::components::{
    BG, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED, NAV_HEIGHT,
    ICON_SERVER, ICON_SETTINGS, ICON_LOGS,
    draw_divider, draw_settings_row,
};

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

    let margin = 16.0;

    // Title "Настройки"
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 32px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Настройки", margin, 20.0);

    let content_x = margin;
    let content_w = w - margin * 2.0;
    let row_h = 56.0;
    let list_top = 72.0;

    struct Item {
        icon: &'static str,
        title: &'static str,
        id: &'static str,
        screen: Screen,
    }

    let items: &[Item] = &[
        Item { icon: ICON_SERVER,   title: "Серверы",          id: "settings_row_0", screen: Screen::SettingsServersList },
        Item { icon: ICON_SETTINGS, title: "Соединение",        id: "settings_row_1", screen: Screen::SettingsConnection  },
        Item { icon: ICON_SETTINGS, title: "Приложение",        id: "settings_row_2", screen: Screen::SettingsApplication },
        Item { icon: ICON_LOGS,     title: "Логи",              id: "settings_row_3", screen: Screen::SettingsLogs        },
        Item { icon: ICON_SETTINGS, title: "О приложении",      id: "settings_row_4", screen: Screen::SettingsAbout       },
    ];

    let list_h = row_h * items.len() as f64;

    // List container
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(content_x, list_top, content_w, list_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(content_x, list_top, content_w, list_h, 16.0);

    let mut navigate_to: Option<Screen> = None;

    for (i, item) in items.iter().enumerate() {
        let row_y = list_top + i as f64 * row_h;

        let row_clicked = draw_settings_row(
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

        if row_clicked {
            navigate_to = Some(item.screen.clone());
        }

        if i < items.len().saturating_sub(1) {
            draw_divider(ctx, content_x + 52.0, row_y + row_h, content_w - 52.0);
        }
    }

    if let Some(screen) = navigate_to {
        state.push_screen(screen);
    }

    // Version footer — sits above nav bar
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Bottom);
    ctx.fill_text("Mirage  v0.1.4", w / 2.0, h - NAV_HEIGHT - 12.0);
}
