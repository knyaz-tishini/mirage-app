//! Main / Home screen — VPN connect button + connection stats + server panel + bottom sheet.

use std::f64::consts::PI;
use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::{Rect, WidgetId};
use uzor::input::{InputState, Sense, LayerId};

use mirage_core::config::ProtocolId;

use crate::app::{AppState, ConnectionState};
use super::components::{
    BG, ACCENT, SURFACE, SURFACE_2, ERROR_COLOR,
    TEXT_PRIMARY, TEXT_MUTED, SEPARATOR, NAV_HEIGHT,
    ICON_CHEVRON_RIGHT, ICON_CHEVRON_DOWN,
    draw_nav_bar,
};

/// Power-off SVG icon (arc + vertical line), 24x24 viewBox.
const ICON_POWER: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18.36 6.64a9 9 0 1 1-12.73 0"/><line x1="12" y1="2" x2="12" y2="12"/></svg>"##;

/// Upload arrow SVG icon (arrow up from base line), 24x24 viewBox.
const ICON_ARROW_UP: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="12" y1="19" x2="12" y2="5"/><polyline points="5 12 12 5 19 12"/></svg>"##;

/// Download arrow SVG icon (arrow down to base line), 24x24 viewBox.
const ICON_ARROW_DOWN: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="12" y1="5" x2="12" y2="19"/><polyline points="19 12 12 19 5 12"/></svg>"##;

/// Server panel height — sits directly above nav bar.
const SERVER_PANEL_H: f64 = 72.0;

/// Stats bar height — shown below the circle when connected.
const STATS_BAR_H: f64 = 52.0;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the human-readable Russian display name for a protocol.
fn protocol_display_name(p: ProtocolId) -> &'static str {
    match p {
        ProtocolId::Mirage       => "Mirage",
        ProtocolId::VlessReality => "VLESS+Reality",
        ProtocolId::Xhttp        => "XHTTP",
        ProtocolId::ShadowTls    => "ShadowTLS",
        ProtocolId::Hysteria2    => "Hysteria2",
    }
}

/// Format bytes as KB / MB / GB string.
fn format_bytes(bytes: u64) -> String {
    if bytes < 1_024 {
        format!("{} Б", bytes)
    } else if bytes < 1_024 * 1_024 {
        format!("{:.1} КБ", bytes as f64 / 1_024.0)
    } else if bytes < 1_024 * 1_024 * 1_024 {
        format!("{:.1} МБ", bytes as f64 / (1_024.0 * 1_024.0))
    } else {
        format!("{:.2} ГБ", bytes as f64 / (1_024.0 * 1_024.0 * 1_024.0))
    }
}

/// Format seconds as HH:MM:SS.
fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

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

    // Top of nav bar — the sheet NEVER overlaps below this.
    let nav_y = h - NAV_HEIGHT;

    // -------------------------------------------------------------------------
    // Background
    // -------------------------------------------------------------------------
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, w, h);

    // -------------------------------------------------------------------------
    // Title bar — "MIRAGE" branding
    // -------------------------------------------------------------------------
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("bold 13px pt root ui");
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
            ctx.set_stroke_color(ACCENT);
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
            ctx.set_stroke_color(TEXT_PRIMARY);
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
    let fill_color = if is_connected { "#15151A" } else { "#121215" };
    ctx.begin_path();
    ctx.arc(cx, cy, r, 0.0, PI * 2.0);
    ctx.set_fill_color(fill_color);
    ctx.fill();

    // Power icon centered in circle
    let icon_size = r * 0.7;
    let icon_color = match &state.connection {
        ConnectionState::Disconnected => TEXT_PRIMARY,
        ConnectionState::Connecting => ACCENT,
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
    let status_y = cy + r + 18.0;
    let (status_text, status_color) = match &state.connection {
        ConnectionState::Disconnected => ("Не защищено", TEXT_MUTED),
        ConnectionState::Connecting => ("Установка соединения...", ACCENT),
        ConnectionState::Connected { .. } => ("Ваше соединение защищено", ACCENT),
        ConnectionState::Error(e) => (e.as_str(), ERROR_COLOR),
    };
    ctx.set_fill_color(status_color);
    ctx.set_font("bold 20px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(status_text, cx, status_y);

    if is_connecting {
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("Нажмите для отмены", cx, status_y + 28.0);
    }

    // -------------------------------------------------------------------------
    // Connection stats bar — shown only when connected
    // -------------------------------------------------------------------------
    if is_connected {
        draw_stats_bar(ctx, w, status_y + 56.0, state);
    }

    // -------------------------------------------------------------------------
    // Server panel — grows directly out of the nav bar.
    // -------------------------------------------------------------------------
    let panel_y = nav_y - SERVER_PANEL_H;
    let panel_pad = 16.0;

    // Resolve selected chain data
    let chain = state
        .config
        .as_ref()
        .and_then(|c| c.chains.get(state.selected_chain));

    let chain_name: String = chain
        .map(|ch| ch.chain_id.clone())
        .unwrap_or_else(|| "Нет сервера".to_string());

    let protocol_name: &'static str = chain
        .map(|ch| protocol_display_name(ch.protocol))
        .unwrap_or("—");

    // Two-hop info: "L1 (sni) → L2 (sni)"
    let hop_info: String = chain
        .map(|ch| {
            let l1 = &ch.l1.sni;
            let l2 = &ch.l2.sni;
            if l1.is_empty() && l2.is_empty() {
                format!("{} → {}", ch.l1.addr, ch.l2.addr)
            } else if l1.is_empty() {
                format!("{} → {l2}", ch.l1.addr)
            } else if l2.is_empty() {
                format!("{l1} → {}", ch.l2.addr)
            } else {
                format!("{l1} → {l2}")
            }
        })
        .unwrap_or_default();

    let panel_rect = Rect::new(0.0, panel_y, w, SERVER_PANEL_H);
    let panel_id = WidgetId::new("server_panel");
    state.coordinator.register(panel_id.clone(), panel_rect, Sense::CLICK);
    let panel_hovered = state.coordinator.is_hovered(&panel_id);

    // Panel background
    let panel_fill = if panel_hovered { SURFACE_2 } else { SURFACE };
    ctx.set_fill_color(panel_fill);
    ctx.fill_rect(0.0, panel_y, w, SERVER_PANEL_H);

    // Thin divider at top of server panel
    ctx.set_stroke_color(SEPARATOR);
    ctx.set_stroke_width(0.5);
    ctx.begin_path();
    ctx.move_to(0.0, panel_y);
    ctx.line_to(w, panel_y);
    ctx.stroke();

    // Chain name (bold, left-aligned)
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 15px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(&chain_name, panel_pad, panel_y + panel_pad);

    // Protocol + hop info (muted, below chain name)
    let detail_str = if hop_info.is_empty() {
        protocol_name.to_string()
    } else {
        format!("{protocol_name}  ·  {hop_info}")
    };
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(&detail_str, panel_pad, panel_y + panel_pad + 22.0);

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
    // Nav bar
    // -------------------------------------------------------------------------
    if let Some(tab) = draw_nav_bar(ctx, input, state, w, h, 0) {
        state.go_to_tab(tab);
    }

    // -------------------------------------------------------------------------
    // Bottom sheet overlay — drawn last, above everything except nav bar.
    // -------------------------------------------------------------------------
    if state.sheet_progress > 0.0 {
        draw_bottom_sheet(ctx, input, w, h, nav_y, state, clicked);
    }
}

// ---------------------------------------------------------------------------
// Stats bar
// ---------------------------------------------------------------------------

/// Draw the connection stats bar (upload / download / duration) below the status label.
fn draw_stats_bar(ctx: &mut dyn RenderContext, w: f64, y: f64, state: &AppState) {
    let bar_w = w - 48.0;
    let bar_x = 24.0;
    let pad = 16.0;

    // Background pill
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(bar_x, y, bar_w, STATS_BAR_H, 12.0);

    // Three columns: [↑ up] | [timer] | [↓ down]
    let col_w = bar_w / 3.0;
    let mid_y = y + STATS_BAR_H / 2.0;
    let icon_sz = 14.0;

    // --- Upload column ---
    let up_str = format_bytes(state.bytes_up);
    let up_x = bar_x + pad;

    draw_svg_icon(ctx, ICON_ARROW_UP, up_x, mid_y - icon_sz / 2.0, icon_sz, icon_sz, ACCENT);

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 13px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(&up_str, up_x + icon_sz + 4.0, mid_y);

    // --- Timer column (centered) ---
    let timer_str = format_duration(state.connection_duration_secs);
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(&timer_str, bar_x + col_w * 1.5, mid_y);

    // --- Download column (right-aligned) ---
    let down_str = format_bytes(state.bytes_down);
    let down_col_right = bar_x + bar_w - pad;
    let down_icon_x = down_col_right - icon_sz - 4.0 - 60.0; // rough width estimate

    draw_svg_icon(ctx, ICON_ARROW_DOWN, down_col_right - icon_sz, mid_y - icon_sz / 2.0, icon_sz, icon_sz, ACCENT);

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 13px pt root ui");
    ctx.set_text_align(TextAlign::Right);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(&down_str, down_col_right - icon_sz - 6.0, mid_y);

    // Suppress unused variable warning from the intermediate calculation
    let _ = down_icon_x;

    // Vertical separators between columns
    for col in 1..3usize {
        let sep_x = bar_x + col_w * col as f64;
        ctx.set_stroke_color(SURFACE_2);
        ctx.set_stroke_width(0.5);
        ctx.begin_path();
        ctx.move_to(sep_x, y + 10.0);
        ctx.line_to(sep_x, y + STATS_BAR_H - 10.0);
        ctx.stroke();
    }
}

// ---------------------------------------------------------------------------
// Bottom sheet
// ---------------------------------------------------------------------------

/// Draw the bottom sheet overlay with server/protocol details.
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

    state.coordinator.push_layer(LayerId::new("sheet"), 100, false);

    let sheet_top_open = h * 0.15;
    let sheet_top = nav_y - (nav_y - sheet_top_open) * progress;
    let sheet_bottom = nav_y;
    let sheet_h = sheet_bottom - sheet_top;

    // Semi-transparent overlay
    let overlay_alpha = 0.5 * progress;
    ctx.set_fill_color_alpha("#000000", overlay_alpha);
    ctx.fill_rect(0.0, 0.0, w, sheet_top);

    // Sheet background with rounded top corners only
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(0.0, sheet_top, w, sheet_h + 16.0, 16.0);
    if sheet_h > 16.0 {
        ctx.fill_rect(0.0, sheet_top + 16.0, w, sheet_h - 16.0);
    }

    // Drag handle
    let handle_w = 40.0;
    let handle_h = 4.0;
    let handle_x = (w - handle_w) / 2.0;
    let handle_y = sheet_top + 12.0;
    ctx.set_fill_color(SURFACE_2);
    ctx.fill_rounded_rect(handle_x, handle_y, handle_w, handle_h, 2.0);

    // Register overlay hit area
    let overlay_rect = Rect::new(0.0, 0.0, w, sheet_top);
    let overlay_id = WidgetId::new("sheet_overlay");
    state.coordinator.register_on_layer(
        overlay_id.clone(),
        overlay_rect,
        Sense::CLICK,
        &LayerId::new("sheet"),
    );

    // Only render content when mostly open
    if progress > 0.5 {
        let content_alpha = ((progress - 0.5) * 2.0).min(1.0);
        ctx.set_global_alpha(content_alpha);

        let pad = 20.0;
        let content_y = sheet_top + 36.0;

        // Resolve selected chain
        let (chain_name, l1_addr, l2_addr, chain_protocol, protocols_on_chain) = {
            let chain = state
                .config
                .as_ref()
                .and_then(|c| c.chains.get(state.selected_chain));

            let name = chain
                .map(|ch| ch.chain_id.clone())
                .unwrap_or_else(|| "Мой сервер".to_string());

            let l1 = chain
                .map(|ch| {
                    if ch.l1.sni.is_empty() {
                        ch.l1.addr.to_string()
                    } else {
                        ch.l1.sni.clone()
                    }
                })
                .unwrap_or_default();

            let l2 = chain
                .map(|ch| {
                    if ch.l2.sni.is_empty() {
                        ch.l2.addr.to_string()
                    } else {
                        ch.l2.sni.clone()
                    }
                })
                .unwrap_or_default();

            let proto = chain.map(|ch| ch.protocol).unwrap_or(ProtocolId::Mirage);

            // Collect all unique protocols across all chains for this server
            let all_protos: Vec<ProtocolId> = state
                .config
                .as_ref()
                .map(|c| {
                    let mut seen = Vec::new();
                    for ch in &c.chains {
                        if !seen.contains(&ch.protocol) {
                            seen.push(ch.protocol);
                        }
                    }
                    seen
                })
                .unwrap_or_default();

            (name, l1, l2, proto, all_protos)
        };

        // Chain name (large bold)
        ctx.set_fill_color(TEXT_PRIMARY);
        ctx.set_font("bold 25px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(&chain_name, w / 2.0, content_y);

        // Two-hop info: "L1 → L2"
        let hop_text = if l1_addr.is_empty() || l2_addr.is_empty() {
            String::new()
        } else {
            format!("{l1_addr}  →  {l2_addr}")
        };

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("13px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        if hop_text.is_empty() {
            ctx.fill_text(protocol_display_name(chain_protocol), w / 2.0, content_y + 34.0);
        } else {
            ctx.fill_text(&hop_text, w / 2.0, content_y + 34.0);
        }

        // Protocols section header
        let protocols_y = content_y + 72.0;
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("bold 14px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text("Протоколы", pad, protocols_y);

        let item_h = 56.0;
        let list_top = protocols_y + 28.0;
        let list_count = protocols_on_chain.len().max(1);

        ctx.set_fill_color(BG);
        ctx.fill_rounded_rect(0.0, list_top, w, item_h * list_count as f64, 12.0);

        if protocols_on_chain.is_empty() {
            // No config loaded — show the active protocol only
            draw_protocol_sheet_item(
                ctx,
                state,
                0,
                protocol_display_name(chain_protocol),
                proto_description(chain_protocol),
                w,
                pad,
                list_top,
                item_h,
                false,
            );
        } else {
            let count = protocols_on_chain.len();
            for (i, &proto) in protocols_on_chain.iter().enumerate() {
                let is_active = proto == chain_protocol;
                draw_protocol_sheet_item(
                    ctx,
                    state,
                    i,
                    protocol_display_name(proto),
                    proto_description(proto),
                    w,
                    pad,
                    list_top + i as f64 * item_h,
                    item_h,
                    i < count.saturating_sub(1),
                );
                // Highlight active protocol with accent text
                if is_active {
                    ctx.set_fill_color(ACCENT);
                    ctx.set_font("bold 11px pt root ui");
                    ctx.set_text_align(TextAlign::Right);
                    ctx.set_text_baseline(TextBaseline::Top);
                    ctx.fill_text("активный", w - pad - 22.0, list_top + i as f64 * item_h + 12.0);
                }
            }
        }

        ctx.set_global_alpha(1.0);
    }

    if clicked && state.coordinator.is_hovered(&WidgetId::new("sheet_overlay")) {
        state.sheet_open = false;
    }
}

/// Draw a single protocol row inside the bottom sheet.
#[allow(clippy::too_many_arguments)]
fn draw_protocol_sheet_item(
    ctx: &mut dyn RenderContext,
    state: &mut AppState,
    idx: usize,
    name: &str,
    desc: &str,
    w: f64,
    pad: f64,
    item_y: f64,
    item_h: f64,
    draw_separator: bool,
) {
    let item_rect = Rect::new(0.0, item_y, w, item_h);
    let item_id = WidgetId::new(format!("sheet_proto_{idx}"));

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

    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 14px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(name, pad, item_y + 10.0);

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(desc, pad, item_y + 30.0);

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

    if draw_separator {
        ctx.set_stroke_color(SEPARATOR);
        ctx.set_stroke_width(0.5);
        ctx.begin_path();
        ctx.move_to(pad, item_y + item_h);
        ctx.line_to(w - pad, item_y + item_h);
        ctx.stroke();
    }
}

/// Short Russian description for each protocol.
fn proto_description(p: ProtocolId) -> &'static str {
    match p {
        ProtocolId::Mirage       => "Нативный протокол — ChaCha20+обфускация",
        ProtocolId::VlessReality => "VLESS с Reality — имитирует HTTPS",
        ProtocolId::Xhttp        => "XHTTP — туннель поверх HTTP",
        ProtocolId::ShadowTls    => "ShadowTLS — маскировка под TLS",
        ProtocolId::Hysteria2    => "Hysteria2 — быстрый UDP/QUIC",
    }
}
