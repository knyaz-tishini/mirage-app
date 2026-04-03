//! Main / Home screen — VPN connect button + server panel + bottom sheet.

use std::f64::consts::PI;
use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::{Rect, WidgetId};
use uzor::input::{InputState, Sense, LayerId};

use crate::app::{AppState, ConnectionState, Screen};
use super::components::{
    BG, ACCENT, SURFACE, SURFACE_2, SUCCESS, WARNING, ERROR_COLOR,
    TEXT_PRIMARY, TEXT_MUTED, SEPARATOR, NAV_HEIGHT,
    ICON_CHEVRON_RIGHT,
    draw_nav_bar,
};

/// Power-off SVG icon (arc + vertical line), 24x24 viewBox.
const ICON_POWER: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18.36 6.64a9 9 0 1 1-12.73 0"/><line x1="12" y1="2" x2="12" y2="12"/></svg>"##;

/// Chevron-down icon for server panel.
const ICON_CHEVRON_DOWN: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"/></svg>"##;

/// Server panel height — sits directly above nav bar.
const SERVER_PANEL_H: f64 = 72.0;

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

    // Top of nav bar — the sheet NEVER overlaps below this.
    let nav_y = h - NAV_HEIGHT;

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
            ctx.begin_path();
            ctx.arc(cx, cy, r + 8.0, 0.0, PI * 2.0);
            ctx.set_stroke_color(ACCENT);
            ctx.set_stroke_width(3.0);
            ctx.stroke();
        }
        ConnectionState::Disconnected => {
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

    // Click detection on the circle — raw distance check, not rect-based
    if clicked {
        if let Some((mx, my)) = input.pointer.pos {
            let dx = mx - cx;
            let dy = my - cy;
            if dx * dx + dy * dy <= (r + 8.0) * (r + 8.0) {
                match &state.connection {
                    ConnectionState::Disconnected | ConnectionState::Error(_) => {
                        state.connect();
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

    if is_connecting {
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("11px sans-serif");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("Tap to cancel", cx, cy + r + 36.0);
    }

    // -------------------------------------------------------------------------
    // Server panel — grows directly out of the nav bar.
    // Both share SURFACE/NAV_BG background, only a thin divider between them.
    // The panel is part of the bottom chrome; it does NOT go behind the sheet.
    // -------------------------------------------------------------------------
    let panel_y = nav_y - SERVER_PANEL_H;
    let panel_pad = 16.0;

    let chain_name = state
        .config
        .as_ref()
        .and_then(|c| c.chains.get(state.selected_chain))
        .map(|ch| ch.chain_id.as_str())
        .unwrap_or("No server");

    let protocol_str = state
        .config
        .as_ref()
        .and_then(|c| c.chains.get(state.selected_chain))
        .map(|ch| ch.protocol.to_string())
        .unwrap_or_else(|| "AmneziaWG".to_string());

    let panel_rect = Rect::new(0.0, panel_y, w, SERVER_PANEL_H);
    let panel_id = WidgetId::new("server_panel");
    state.coordinator.register(panel_id.clone(), panel_rect, Sense::CLICK);
    let panel_hovered = state.coordinator.is_hovered(&panel_id);

    // Panel background — same as nav bar so they merge visually
    let panel_fill = if panel_hovered { SURFACE_2 } else { SURFACE };
    ctx.set_fill_color(panel_fill);
    ctx.fill_rect(0.0, panel_y, w, SERVER_PANEL_H);

    // Thin divider at top of server panel (separates it from content area)
    ctx.set_stroke_color(SEPARATOR);
    ctx.set_stroke_width(0.5);
    ctx.begin_path();
    ctx.move_to(0.0, panel_y);
    ctx.line_to(w, panel_y);
    ctx.stroke();

    // Chain name (bold, left-aligned)
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 15px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(chain_name, panel_pad, panel_y + panel_pad);

    // Protocol (muted, below chain name)
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("12px sans-serif");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(&protocol_str, panel_pad, panel_y + panel_pad + 22.0);

    // Chevron down on the right
    let chevron_size = 20.0;
    draw_svg_icon(
        ctx,
        ICON_CHEVRON_DOWN,
        w - chevron_size - panel_pad,
        panel_y + (SERVER_PANEL_H - chevron_size) / 2.0,
        chevron_size,
        chevron_size,
        TEXT_MUTED,
    );

    // Click on server panel toggles the bottom sheet
    if clicked && panel_hovered {
        state.sheet_open = !state.sheet_open;
    }

    // -------------------------------------------------------------------------
    // Nav bar — drawn over the server panel continuation
    // -------------------------------------------------------------------------
    let active_tab = 0;
    if let Some(tab) = draw_nav_bar(ctx, input, state, w, h, active_tab) {
        navigate_tab(state, tab);
    }

    // -------------------------------------------------------------------------
    // Bottom sheet overlay — drawn last, above everything except nav bar.
    // Sheet STOPS at nav_y: it never covers the nav bar.
    // -------------------------------------------------------------------------
    if state.sheet_progress > 0.0 {
        draw_bottom_sheet(ctx, input, w, h, nav_y, state, clicked);
    }
}

/// Draw the bottom sheet overlay with server/protocol details.
///
/// The sheet rises from `nav_y` (top of nav bar) upward.
/// At progress = 1.0 the sheet top is at `h * 0.15`.
/// The sheet NEVER extends below `nav_y` — the nav bar always stays visible.
fn draw_bottom_sheet(
    ctx: &mut dyn RenderContext,
    _input: &InputState,
    w: f64,
    h: f64,
    nav_y: f64,
    state: &mut AppState,
    clicked: bool,
) {
    let progress = state.sheet_progress;

    // Push a sheet layer so sheet widgets win over background widgets in z-order.
    state.coordinator.push_layer(LayerId::new("sheet"), 100, false);

    // Sheet top position: lerps from nav_y (fully closed) to h*0.15 (fully open).
    let sheet_top_open = h * 0.15;
    let sheet_top = nav_y - (nav_y - sheet_top_open) * progress;
    let sheet_bottom = nav_y; // Always stops at nav bar top edge
    let sheet_h = sheet_bottom - sheet_top;

    // Semi-transparent overlay covers the area ABOVE the sheet (between title bar and sheet)
    let overlay_alpha = 0.5 * progress;
    ctx.set_fill_color_alpha("#000000", overlay_alpha);
    ctx.fill_rect(0.0, 0.0, w, sheet_top);

    // Sheet background with rounded top corners only
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(0.0, sheet_top, w, sheet_h + 16.0, 16.0);
    // Fill the lower portion square so only the top corners are rounded
    if sheet_h > 16.0 {
        ctx.fill_rect(0.0, sheet_top + 16.0, w, sheet_h - 16.0);
    }

    // Drag handle — small pill at the top of the sheet
    let handle_w = 40.0;
    let handle_h = 4.0;
    let handle_x = (w - handle_w) / 2.0;
    let handle_y = sheet_top + 12.0;
    ctx.set_fill_color(SURFACE_2);
    ctx.fill_rounded_rect(handle_x, handle_y, handle_w, handle_h, 2.0);

    // Register sheet overlay hit area so clicks above sheet are captured
    let overlay_rect = Rect::new(0.0, 0.0, w, sheet_top);
    let overlay_id = WidgetId::new("sheet_overlay");
    state.coordinator.register_on_layer(overlay_id.clone(), overlay_rect, Sense::CLICK, &LayerId::new("sheet"));

    // Only render sheet content when mostly open
    if progress > 0.5 {
        let content_alpha = ((progress - 0.5) * 2.0).min(1.0);
        ctx.set_global_alpha(content_alpha);

        let pad = 20.0;
        let content_y = sheet_top + 36.0;

        // Server name (large bold)
        let chain_name = state
            .config
            .as_ref()
            .and_then(|c| c.chains.get(state.selected_chain))
            .map(|ch| ch.chain_id.as_str())
            .unwrap_or("My Server");

        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("bold 18px sans-serif");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(chain_name, pad, content_y);

        // Server address (muted below name)
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px sans-serif");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("vpn.example.com", pad, content_y + 26.0);

        // Protocols section header
        let protocols_y = content_y + 66.0;
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("11px sans-serif");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("PROTOCOLS", pad, protocols_y);

        // Protocol items — hardcoded Amnezia-style list
        struct Protocol {
            name: &'static str,
            desc: &'static str,
        }
        let protocols = [
            Protocol { name: "AmneziaWG",  desc: "Obfuscated WireGuard — recommended" },
            Protocol { name: "WireGuard",  desc: "Fast, modern VPN protocol"           },
            Protocol { name: "XRay",       desc: "Traffic obfuscation via XTLS/Reality" },
        ];

        let item_h = 60.0;
        let list_top = protocols_y + 20.0;

        // List card background
        ctx.set_fill_color(BG);
        ctx.fill_rounded_rect(0.0, list_top, w, item_h * protocols.len() as f64, 12.0);

        for (i, proto) in protocols.iter().enumerate() {
            let item_y = list_top + i as f64 * item_h;
            let item_rect = Rect::new(0.0, item_y, w, item_h);
            let item_id = WidgetId::new(format!("sheet_proto_{i}"));

            state.coordinator.register_on_layer(
                item_id.clone(),
                item_rect,
                Sense::CLICK,
                &LayerId::new("sheet"),
            );
            let item_hovered = state.coordinator.is_hovered(&item_id);

            if item_hovered {
                ctx.set_fill_color_alpha(SURFACE_2, 0.4);
                ctx.fill_rect(0.0, item_y, w, item_h);
            }

            // Protocol name
            ctx.set_fill_color(TEXT_PRIMARY);
            ctx.set_font("bold 14px sans-serif");
            ctx.set_text_align(TextAlign::Left);
            ctx.set_text_baseline(TextBaseline::Top);
            ctx.fill_text(proto.name, pad, item_y + 12.0);

            // Protocol description
            ctx.set_fill_color(TEXT_MUTED);
            ctx.set_font("12px sans-serif");
            ctx.set_text_baseline(TextBaseline::Top);
            ctx.fill_text(proto.desc, pad, item_y + 32.0);

            // Chevron right
            let chevron_size = 18.0;
            draw_svg_icon(
                ctx,
                ICON_CHEVRON_RIGHT,
                w - chevron_size - pad,
                item_y + (item_h - chevron_size) / 2.0,
                chevron_size,
                chevron_size,
                TEXT_MUTED,
            );

            // Divider between items (not after last)
            if i < protocols.len().saturating_sub(1) {
                ctx.set_stroke_color(SEPARATOR);
                ctx.set_stroke_width(0.5);
                ctx.begin_path();
                ctx.move_to(pad, item_y + item_h);
                ctx.line_to(w - pad, item_y + item_h);
                ctx.stroke();
            }
        }

        ctx.set_global_alpha(1.0);
    }

    // Clicking the overlay (above the sheet) closes the sheet
    if clicked && state.coordinator.is_hovered(&WidgetId::new("sheet_overlay")) {
        state.sheet_open = false;
    }
}

pub fn navigate_tab(state: &mut AppState, tab: usize) {
    let screen = match tab {
        0 => Screen::Main,
        1 => Screen::Chains,
        2 => Screen::Settings,
        3 => Screen::Connect,
        _ => return,
    };
    state.go_to(screen);
}
