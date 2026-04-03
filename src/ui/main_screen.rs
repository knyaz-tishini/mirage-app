//! Main / Home screen — VPN connect button + server panel.

use std::f64::consts::PI;
use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::Rect;
use uzor::input::InputState;

use crate::app::{AppState, ConnectionState, Screen};
use super::components::{
    BG, ACCENT, SUCCESS, WARNING, ERROR_COLOR,
    TEXT_PRIMARY, TEXT_MUTED, SEPARATOR, NAV_HEIGHT,
    ICON_CHEVRON_RIGHT,
    draw_nav_bar,
};

/// Draw the main screen.
pub fn draw_main_screen(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    viewport: Rect,
    state: &mut AppState,
) {
    let w = viewport.width;
    let h = viewport.height;
    let clicked = input.is_clicked();
    let mouse = input.pointer.pos;

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
    // Big circle button — centered at ~280px from top
    // -------------------------------------------------------------------------
    let cx = w / 2.0;
    let cy = 280.0;
    let r = 90.0;

    let is_connected = state.is_connected();
    let is_connecting = matches!(state.connection, ConnectionState::Connecting);

    // Ring stroke color
    let ring_color = match &state.connection {
        ConnectionState::Disconnected => SEPARATOR,
        ConnectionState::Connecting => WARNING,
        ConnectionState::Connected { .. } => ACCENT,
        ConnectionState::Error(_) => ERROR_COLOR,
    };

    // Outer ring (2px stroke)
    ctx.begin_path();
    ctx.arc(cx, cy, r + 4.0, 0.0, PI * 2.0);
    ctx.set_stroke_color(ring_color);
    ctx.set_stroke_width(2.0);
    ctx.stroke();

    // Inner fill (slightly lighter than BG)
    let fill_color = if is_connected && !is_connecting {
        "#15151A"
    } else {
        "#121215"
    };
    ctx.begin_path();
    ctx.arc(cx, cy, r, 0.0, PI * 2.0);
    ctx.set_fill_color(fill_color);
    ctx.fill();

    // Circle inner text
    let (circle_text, text_color) = match &state.connection {
        ConnectionState::Disconnected => ("TAP TO\nCONNECT", TEXT_MUTED),
        ConnectionState::Connecting => ("CONNECTING", WARNING),
        ConnectionState::Connected { .. } => ("CONNECTED", ACCENT),
        ConnectionState::Error(_) => ("ERROR\nTAP TO RETRY", ERROR_COLOR),
    };

    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.set_fill_color(text_color);

    let lines: Vec<&str> = circle_text.split('\n').collect();
    let line_h = 18.0;
    let total_h = lines.len() as f64 * line_h;
    let start_y = cy - total_h / 2.0 + line_h / 2.0;

    for (i, line) in lines.iter().enumerate() {
        let font = if i == 0 {
            "bold 14px sans-serif"
        } else {
            "12px sans-serif"
        };
        ctx.set_font(font);
        ctx.fill_text(line, cx, start_y + i as f64 * line_h);
    }

    // Click detection on the circle
    if clicked {
        if let Some((mx, my)) = mouse {
            let dx = mx - cx;
            let dy = my - cy;
            if dx * dx + dy * dy <= (r + 4.0) * (r + 4.0) {
                match &state.connection {
                    ConnectionState::Disconnected | ConnectionState::Error(_) => {
                        state.connect();
                        state.on_connected();
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

    // -------------------------------------------------------------------------
    // Server selector panel (bottom, above nav bar)
    // Amnezia-style: transparent bg with top border, left-aligned server name
    // -------------------------------------------------------------------------
    let panel_margin = 16.0;
    let panel_y = h - NAV_HEIGHT - 90.0;
    let panel_h = 68.0;
    let panel_x = panel_margin;
    let panel_w = w - panel_margin * 2.0;

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

    let server_addr = state
        .config
        .as_ref()
        .and_then(|c| c.chains.get(state.selected_chain))
        .map(|ch| ch.l1.addr.to_string())
        .unwrap_or_else(|| "—".to_string());

    let panel_hovered = mouse
        .map(|(mx, my)| Rect::new(panel_x, panel_y, panel_w, panel_h).contains(mx, my))
        .unwrap_or(false);

    // Panel background (slightly lighter on hover)
    if panel_hovered {
        ctx.set_fill_color_alpha(SEPARATOR, 0.4);
        ctx.fill_rounded_rect(panel_x, panel_y, panel_w, panel_h, 12.0);
    }

    // Top border line
    ctx.set_stroke_color(SEPARATOR);
    ctx.set_stroke_width(0.5);
    ctx.begin_path();
    ctx.move_to(panel_x, panel_y);
    ctx.line_to(panel_x + panel_w, panel_y);
    ctx.stroke();

    // Chain name (left)
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 15px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(chain_name, panel_x + 4.0, panel_y + 10.0);

    // Chevron right SVG (right side)
    let chevron_size = 16.0;
    draw_svg_icon(
        ctx,
        ICON_CHEVRON_RIGHT,
        panel_x + panel_w - chevron_size - 4.0,
        panel_y + (panel_h - chevron_size) / 2.0,
        chevron_size,
        chevron_size,
        ACCENT,
    );

    // Protocol + addr
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("12px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    let detail = format!("{protocol_str}  ·  {server_addr}");
    ctx.fill_text(&detail, panel_x + 4.0, panel_y + 32.0);

    // Click navigates to chains screen
    if clicked && panel_hovered {
        state.go_to(Screen::Chains);
    }

    // -------------------------------------------------------------------------
    // Nav bar
    // -------------------------------------------------------------------------
    let active_tab = 0;
    if let Some(tab) = draw_nav_bar(ctx, w, h, active_tab, mouse, clicked) {
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
