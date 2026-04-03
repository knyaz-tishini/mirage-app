//! Main / Home screen — VPN connect button + server panel.

use std::f64::consts::PI;
use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::Rect;
use uzor::input::InputState;

use crate::app::{AppState, ConnectionState, Screen};
use super::components::{
    BG, ACCENT, SURFACE, SURFACE_2, SUCCESS, WARNING, ERROR_COLOR,
    TEXT_PRIMARY, TEXT_MUTED, SEPARATOR, NAV_HEIGHT,
    ICON_CHEVRON_RIGHT,
    draw_nav_bar,
};

/// Power-off SVG icon (arc + vertical line), 24x24 viewBox.
const ICON_POWER: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18.36 6.64a9 9 0 1 1-12.73 0"/><line x1="12" y1="2" x2="12" y2="12"/></svg>"##;

/// Draw the main screen.
pub fn draw_main_screen(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    viewport: Rect,
    state: &mut AppState,
) {
    // Advance animation + connection timer first.
    state.tick(input.dt);

    let w = viewport.width;
    let h = viewport.height;
    let clicked = input.is_clicked();

    // -------------------------------------------------------------------------
    // Background
    // -------------------------------------------------------------------------
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, w, h);

    // -------------------------------------------------------------------------
    // Title bar
    // -------------------------------------------------------------------------
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("bold 13px sans-serif");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("MIRAGE", w / 2.0, 20.0);

    // -------------------------------------------------------------------------
    // Big circle button — proportional position
    // -------------------------------------------------------------------------
    let cx = w / 2.0;
    let cy = h * 0.36;
    let r = (w.min(h) * 0.17).min(90.0);

    let is_connecting = matches!(state.connection, ConnectionState::Connecting);
    let is_connected = matches!(state.connection, ConnectionState::Connected { .. });

    // Outer ring — state-dependent
    match &state.connection {
        ConnectionState::Connecting => {
            // Pulsing ring at r+8
            let alpha = 0.3 + 0.7 * ((state.animation_phase * std::f64::consts::TAU).sin().abs());
            ctx.set_global_alpha(alpha);
            ctx.begin_path();
            ctx.arc(cx, cy, r + 8.0, 0.0, PI * 2.0);
            ctx.set_stroke_color(WARNING);
            ctx.set_stroke_width(3.0);
            ctx.stroke();
            ctx.set_global_alpha(1.0);
        }
        ConnectionState::Connected { .. } => {
            // Solid accent outer ring
            ctx.begin_path();
            ctx.arc(cx, cy, r + 8.0, 0.0, PI * 2.0);
            ctx.set_stroke_color(ACCENT);
            ctx.set_stroke_width(3.0);
            ctx.stroke();
        }
        ConnectionState::Disconnected => {
            // Subtle SURFACE_2 ring
            ctx.begin_path();
            ctx.arc(cx, cy, r + 8.0, 0.0, PI * 2.0);
            ctx.set_stroke_color(SURFACE_2);
            ctx.set_stroke_width(2.0);
            ctx.stroke();
        }
        ConnectionState::Error(_) => {
            ctx.begin_path();
            ctx.arc(cx, cy, r + 8.0, 0.0, PI * 2.0);
            ctx.set_stroke_color(ERROR_COLOR);
            ctx.set_stroke_width(2.0);
            ctx.stroke();
        }
    }

    // Inner fill
    let fill_color = if is_connected {
        "#15151A"
    } else {
        "#121215"
    };
    ctx.begin_path();
    ctx.arc(cx, cy, r, 0.0, PI * 2.0);
    ctx.set_fill_color(fill_color);
    ctx.fill();

    // Power icon centered in circle
    let icon_size = r * 0.7;
    let icon_color = match &state.connection {
        ConnectionState::Disconnected => TEXT_MUTED,
        ConnectionState::Connecting => WARNING,
        ConnectionState::Connected { .. } => ACCENT,
        ConnectionState::Error(_) => ERROR_COLOR,
    };
    draw_svg_icon(
        ctx,
        ICON_POWER,
        cx - icon_size / 2.0,
        cy - icon_size / 2.0,
        icon_size,
        icon_size,
        icon_color,
    );

    // Click detection on the circle
    if clicked {
        if let Some((mx, my)) = input.pointer.pos {
            let dx = mx - cx;
            let dy = my - cy;
            if dx * dx + dy * dy <= (r + 8.0) * (r + 8.0) {
                match &state.connection {
                    ConnectionState::Disconnected | ConnectionState::Error(_) => {
                        state.connect();
                        // tick() drives the Connecting → Connected transition after 1.5 s
                    }
                    ConnectionState::Connected { .. } => {
                        state.disconnect();
                    }
                    ConnectionState::Connecting => {}
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // Status label below circle
    // -------------------------------------------------------------------------
    let (status_text, status_color) = match &state.connection {
        ConnectionState::Disconnected => ("Not protected", TEXT_MUTED),
        ConnectionState::Connecting => ("Establishing connection...", WARNING),
        ConnectionState::Connected { .. } => ("Your connection is secure", SUCCESS),
        ConnectionState::Error(e) => (e.as_str(), ERROR_COLOR),
    };
    ctx.set_fill_color(status_color);
    ctx.set_font("13px sans-serif");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(status_text, cx, cy + r + 18.0);

    // Connecting sub-label (small hint)
    if is_connecting {
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("11px sans-serif");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("Tap to cancel", cx, cy + r + 36.0);
    }

    // -------------------------------------------------------------------------
    // Server selector card (bottom, above nav bar)
    // Proper card with SURFACE fill, rounded corners, padding
    // -------------------------------------------------------------------------
    let card_margin = 16.0;
    let card_y = h - NAV_HEIGHT - 96.0;
    let card_h = 72.0;
    let card_x = card_margin;
    let card_w = w - card_margin * 2.0;
    let card_pad = 16.0;

    let chain_name = state
        .config
        .as_ref()
        .and_then(|c| c.chains.get(state.selected_chain))
        .map(|ch| ch.chain_id.as_str())
        .unwrap_or("No config");

    let protocol_str = state
        .config
        .as_ref()
        .and_then(|c| c.chains.get(state.selected_chain))
        .map(|ch| ch.protocol.to_string())
        .unwrap_or_else(|| "—".to_string());

    let card_rect = Rect::new(card_x, card_y, card_w, card_h);
    let card_hovered = input.is_hovered(&card_rect);

    // Card background
    let card_fill = if card_hovered { SURFACE_2 } else { SURFACE };
    ctx.set_fill_color(card_fill);
    ctx.fill_rounded_rect(card_x, card_y, card_w, card_h, 12.0);

    // Card border
    ctx.set_stroke_color(SEPARATOR);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(card_x, card_y, card_w, card_h, 12.0);

    // Chain name (bold, left-aligned with padding)
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 15px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(chain_name, card_x + card_pad, card_y + card_pad);

    // Protocol (muted, below chain name)
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("12px sans-serif");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(&protocol_str, card_x + card_pad, card_y + card_pad + 22.0);

    // Chevron right SVG (right side)
    let chevron_size = 16.0;
    draw_svg_icon(
        ctx,
        ICON_CHEVRON_RIGHT,
        card_x + card_w - chevron_size - card_pad,
        card_y + (card_h - chevron_size) / 2.0,
        chevron_size,
        chevron_size,
        ACCENT,
    );

    // Click navigates to chains screen
    if clicked && card_hovered {
        state.go_to(Screen::Chains);
    }

    // -------------------------------------------------------------------------
    // Nav bar
    // -------------------------------------------------------------------------
    let active_tab = 0;
    if let Some(tab) = draw_nav_bar(ctx, input, w, h, active_tab) {
        navigate_tab(state, tab);
    }
}

pub fn navigate_tab(state: &mut AppState, tab: usize) {
    let screen = match tab {
        0 => Screen::Main,
        1 => Screen::Chains,
        2 => Screen::Settings,
        3 => Screen::Logs,
        _ => return,
    };
    state.go_to(screen);
}
