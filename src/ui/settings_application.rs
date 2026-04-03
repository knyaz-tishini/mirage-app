//! Settings → Приложение screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::Rect;

use crate::app::AppState;
use super::components::{
    BG, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED,
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

    if draw_back_header(ctx, input, state, "Приложение", w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let content_w = w - margin * 2.0;
    let mut y = 80.0;

    // Toggles
    struct Toggle {
        title: &'static str,
        subtitle: &'static str,
    }

    let toggles = [
        Toggle { title: "Язык",             subtitle: "Русский"           },
        Toggle { title: "Включить логи",     subtitle: "Запись событий"    },
        Toggle { title: "Автозапуск",        subtitle: "При запуске системы" },
        Toggle { title: "Автоподключение",   subtitle: "Подключаться автоматически" },
    ];

    let item_h = 64.0;
    let list_h = item_h * toggles.len() as f64;

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, list_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, list_h, 16.0);

    for (i, toggle) in toggles.iter().enumerate() {
        let item_y = y + i as f64 * item_h;

        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("16px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(toggle.title, margin + 16.0, item_y + item_h / 2.0 - 1.0);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(toggle.subtitle, margin + 16.0, item_y + item_h / 2.0 + 3.0);

        // Toggle pill (placeholder off)
        let pill_w = 44.0;
        let pill_h = 26.0;
        let pill_x = margin + content_w - pill_w - 16.0;
        let pill_y = item_y + (item_h - pill_h) / 2.0;
        ctx.set_fill_color(SURFACE_2);
        ctx.fill_rounded_rect(pill_x, pill_y, pill_w, pill_h, 13.0);

        if i < toggles.len().saturating_sub(1) {
            draw_divider(ctx, margin + 16.0, item_y + item_h, content_w - 16.0);
        }
    }

    y += list_h + 20.0;

    // "Сбросить настройки" — destructive
    let btn_h = 52.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, btn_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, btn_h, 12.0);

    ctx.set_fill_color("#EB5757");
    ctx.set_font("15px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(
        "Сбросить настройки и удалить все данные",
        margin + 16.0,
        y + btn_h / 2.0,
    );

    let _ = h;
}
