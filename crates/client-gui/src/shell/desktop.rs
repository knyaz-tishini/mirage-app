//! Desktop shell chrome: titlebar, minimize/close buttons, M-menu button.

use uzor::render::RenderContext;
use uzor::render::{TextAlign, TextBaseline};

/// Height of the custom titlebar in logical pixels.
pub const TITLEBAR_H: f64 = 36.0;
const BTN_W: f64 = 44.0;

/// Hover state for the titlebar buttons.
#[derive(Default, Clone, Copy)]
pub struct TitlebarHover {
    pub minimize: bool,
    pub close: bool,
    pub menu: bool,
}

/// Draw the desktop titlebar chrome.
///
/// Paints: background, bottom border, M-menu glyph, minimize/close controls.
/// No text wordmarks or status labels — those were removed.
pub fn draw_titlebar(
    ctx: &mut dyn RenderContext,
    hover: TitlebarHover,
    w: f64,
    menu_open: bool,
) {
    // Titlebar background.
    ctx.set_fill_color("#0C0C10");
    ctx.fill_rect(0.0, 0.0, w, TITLEBAR_H);

    // Bottom border.
    ctx.set_stroke_color("rgba(255,255,255,0.08)");
    ctx.set_stroke_width(1.0);
    ctx.begin_path();
    ctx.move_to(0.0, TITLEBAR_H);
    ctx.line_to(w, TITLEBAR_H);
    ctx.stroke();

    // M-button.
    if hover.menu || menu_open {
        ctx.set_fill_color("rgba(251,178,106,0.18)");
        ctx.fill_rect(4.0, 4.0, 44.0, TITLEBAR_H - 8.0);
    }
    ctx.set_fill_color("#FBB26A");
    ctx.set_font("bold 18px DM Sans");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text("M", 26.0, TITLEBAR_H / 2.0);

    // Minimize button.
    let min_x = w - BTN_W * 2.0;
    if hover.minimize {
        ctx.set_fill_color("rgba(255,255,255,0.07)");
        ctx.fill_rect(min_x, 0.0, BTN_W, TITLEBAR_H);
    }
    let cy = TITLEBAR_H / 2.0;
    ctx.set_stroke_color(if hover.minimize { "#F4F4F5" } else { "#555860" });
    ctx.set_stroke_width(1.2);
    ctx.begin_path();
    ctx.move_to(min_x + BTN_W / 2.0 - 6.0, cy);
    ctx.line_to(min_x + BTN_W / 2.0 + 6.0, cy);
    ctx.stroke();

    // Close button.
    let close_x = w - BTN_W;
    if hover.close {
        ctx.set_fill_color("rgba(224,86,86,0.6)");
        ctx.fill_rect(close_x, 0.0, BTN_W, TITLEBAR_H);
    }
    let cxh = close_x + BTN_W / 2.0;
    ctx.set_stroke_color(if hover.close { "#F4F4F5" } else { "#555860" });
    ctx.set_stroke_width(1.2);
    ctx.begin_path();
    ctx.move_to(cxh - 5.0, cy - 5.0);
    ctx.line_to(cxh + 5.0, cy + 5.0);
    ctx.move_to(cxh + 5.0, cy - 5.0);
    ctx.line_to(cxh - 5.0, cy + 5.0);
    ctx.stroke();
}
