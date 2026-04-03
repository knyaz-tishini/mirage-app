//! Settings → Приложение screen.

use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::{Rect, WidgetId};
use uzor::input::Sense;

use crate::app::{AppState, ConfirmDrawer, Screen};
use super::components::{
    BG, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED, ERROR_COLOR, ICON_LOGS, ICON_CHEVRON_RIGHT,
    draw_back_header, draw_divider, draw_toggle,
};
use uzor::render::draw_svg_icon;

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
    let row_h = 60.0;
    let clicked = input.is_clicked();

    // ── Toggles section ────────────────────────────────────────────────────
    struct ToggleRow {
        id: &'static str,
        title: &'static str,
        subtitle: &'static str,
    }

    let toggle_rows: &[ToggleRow] = &[
        ToggleRow { id: "autostart",     title: "Автозапуск",          subtitle: "При старте системы"           },
        ToggleRow { id: "autoconnect",   title: "Автоподключение",      subtitle: "Подключаться автоматически"   },
        ToggleRow { id: "minimize_tray", title: "Сворачивать в трей",   subtitle: "Скрывать в системный трей"    },
        ToggleRow { id: "notifications", title: "Уведомления",          subtitle: "Push-уведомления о событиях"  },
    ];

    let toggles_h = row_h * toggle_rows.len() as f64;

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, toggles_h, 16.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, toggles_h, 16.0);

    for (i, row) in toggle_rows.iter().enumerate() {
        let row_y = y + i as f64 * row_h;
        let mid_y = row_y + row_h / 2.0;

        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("15px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(row.title, margin + 16.0, mid_y - 1.0);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("12px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(row.subtitle, margin + 16.0, mid_y + 3.0);

        let pill_w = 44.0;
        let pill_h = 26.0;
        let pill_x = margin + content_w - pill_w - 16.0;
        let pill_y = row_y + (row_h - pill_h) / 2.0;
        draw_toggle(ctx, input, state, row.id, pill_x, pill_y);

        if i < toggle_rows.len().saturating_sub(1) {
            draw_divider(ctx, margin + 16.0, row_y + row_h, content_w - 16.0);
        }
    }

    y += toggles_h + 16.0;

    // ── Divider label ──────────────────────────────────────────────────────
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("ДОПОЛНИТЕЛЬНО", margin + 4.0, y);
    y += 20.0;

    // ── Logs row ───────────────────────────────────────────────────────────
    let extra_rows_h = row_h;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, extra_rows_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, extra_rows_h, 12.0);

    let logs_id = WidgetId::new("app_logs_row");
    let logs_rect = Rect::new(margin, y, content_w, row_h);
    state.coordinator.register(logs_id.clone(), logs_rect, Sense::CLICK);
    let logs_hovered = state.coordinator.is_hovered(&logs_id);

    if logs_hovered {
        ctx.set_fill_color_alpha(SURFACE_2, 0.4);
        ctx.fill_rounded_rect(margin, y, content_w, row_h, 12.0);
    }

    let icon_size = 24.0;
    let icon_cy = y + row_h / 2.0;
    draw_svg_icon(ctx, ICON_LOGS, margin + 16.0, icon_cy - icon_size / 2.0, icon_size, icon_size, TEXT_MUTED);

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("15px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Логи", margin + 16.0 + icon_size + 14.0, icon_cy);

    let chev_size = 20.0;
    draw_svg_icon(ctx, ICON_CHEVRON_RIGHT, margin + content_w - chev_size - 14.0, icon_cy - chev_size / 2.0, chev_size, chev_size, TEXT_MUTED);

    if clicked && logs_hovered {
        state.push_screen(Screen::SettingsLogs);
        return;
    }

    y += extra_rows_h + 16.0;

    // ── Reset settings — destructive button ────────────────────────────────
    let btn_h = 52.0;
    let reset_id = WidgetId::new("app_reset_btn");
    let reset_rect = Rect::new(margin, y, content_w, btn_h);
    state.coordinator.register(reset_id.clone(), reset_rect, Sense::CLICK);
    let reset_hovered = state.coordinator.is_hovered(&reset_id);

    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, btn_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, btn_h, 12.0);

    if reset_hovered {
        ctx.set_fill_color_alpha(ERROR_COLOR, 0.08);
        ctx.fill_rounded_rect(margin, y, content_w, btn_h, 12.0);
    }

    ctx.set_fill_color(ERROR_COLOR);
    ctx.set_font("15px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("Сбросить настройки", margin + 16.0, y + btn_h / 2.0);

    if clicked && reset_hovered {
        state.show_confirm(ConfirmDrawer {
            title: "Сбросить все настройки?".into(),
            description: "Все данные будут удалены без возможности восстановления.".into(),
            confirm_label: "Сбросить".into(),
            cancel_label: "Отмена".into(),
            action_id: "reset_all_settings".into(),
        });
    }

    let _ = h;
}
