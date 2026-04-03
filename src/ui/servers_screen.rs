//! Servers screen — server list with chain name, protocol badge, status dot, and scroll.

use uzor::render::{RenderContext, TextAlign, TextBaseline, draw_svg_icon};
use uzor::types::{Rect, WidgetId};
use uzor::input::{InputState, Sense};

use mirage_core::config::ProtocolId;

use crate::app::{AppState, Screen};
use super::components::{
    ACCENT, BG, SURFACE, SURFACE_2, TEXT_PRIMARY, TEXT_MUTED, SEPARATOR, NAV_HEIGHT,
    SUCCESS, ERROR_COLOR,
    ICON_SETTINGS, ICON_PLUS,
    draw_nav_bar, draw_divider,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Human-readable Russian display name for a protocol.
fn protocol_display_name(p: ProtocolId) -> &'static str {
    match p {
        ProtocolId::Mirage       => "Mirage",
        ProtocolId::VlessReality => "VLESS+Reality",
        ProtocolId::Xhttp        => "XHTTP",
        ProtocolId::ShadowTls    => "ShadowTLS",
        ProtocolId::Hysteria2    => "Hysteria2",
    }
}

/// Simulated node status per chain index for demo purposes.
///
/// In a real implementation this would come from mirage-mesh `NodeStatus`.
/// Cycles through Online / Degraded / Offline / Provisioning based on index.
#[derive(Clone, Copy, PartialEq)]
enum DisplayStatus {
    Online,
    Degraded,
    Offline,
    Provisioning,
}

impl DisplayStatus {
    fn color(self) -> &'static str {
        match self {
            Self::Online       => SUCCESS,
            Self::Degraded     => "#FBB26A", // ACCENT orange = "degraded"
            Self::Offline      => ERROR_COLOR,
            Self::Provisioning => "#878B91", // TEXT_MUTED gray
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Online       => "Онлайн",
            Self::Degraded     => "Деградация",
            Self::Offline      => "Офлайн",
            Self::Provisioning => "Инициализация",
        }
    }

    /// Derive a status from the chain index (placeholder until real health data arrives).
    fn for_index(idx: usize) -> Self {
        match idx % 4 {
            0 => Self::Online,
            1 => Self::Degraded,
            2 => Self::Offline,
            _ => Self::Provisioning,
        }
    }
}

// ---------------------------------------------------------------------------
// Protocol badge
// ---------------------------------------------------------------------------

/// Draw a small colored badge with the protocol name.
fn draw_protocol_badge(
    ctx: &mut dyn RenderContext,
    label: &str,
    x: f64,
    y: f64,
) {
    let pad_h = 5.0;
    let pad_v = 3.0;
    // Approximate text width: ~6.5 px per char at 10px font
    let text_w = label.len() as f64 * 6.5;
    let badge_w = text_w + pad_h * 2.0;
    let badge_h = 16.0 + pad_v * 2.0;

    ctx.set_fill_color_alpha(ACCENT, 0.18);
    ctx.fill_rounded_rect(x, y, badge_w, badge_h, 4.0);

    ctx.set_fill_color(ACCENT);
    ctx.set_font("bold 10px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(label, x + pad_h, y + pad_v);
}

/// Returns the pixel width of the badge for layout calculations.
fn badge_width(label: &str) -> f64 {
    label.len() as f64 * 6.5 + 10.0
}

// ---------------------------------------------------------------------------
// Status dot
// ---------------------------------------------------------------------------

/// Draw a small filled circle indicating node status.
fn draw_status_dot(ctx: &mut dyn RenderContext, cx: f64, cy: f64, status: DisplayStatus) {
    let r = 5.0;
    ctx.begin_path();
    ctx.arc(cx, cy, r, 0.0, std::f64::consts::PI * 2.0);
    ctx.set_fill_color(status.color());
    ctx.fill();
}

// ---------------------------------------------------------------------------
// Screen
// ---------------------------------------------------------------------------

pub fn draw_servers_screen(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    viewport: Rect,
    state: &mut AppState,
) {
    let w = viewport.width;
    let h = viewport.height;
    let clicked = input.is_clicked();

    // Background
    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, w, h);

    let margin = 16.0;

    // Header "Серверы"
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 32px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text("Серверы", margin, 20.0);

    let content_top = 72.0;
    let content_bottom = h - NAV_HEIGHT - 8.0;
    let content_w = w - margin * 2.0;

    // "Добавить сервер" button at bottom — layout first so list doesn't overlap it
    let add_btn_h = 52.0;
    let add_btn_y = content_bottom - add_btn_h;

    let list_top = content_top;
    let item_h = 80.0;

    // -------------------------------------------------------------------------
    // Server list
    // -------------------------------------------------------------------------
    if let Some(cfg) = &state.config {
        if cfg.chains.is_empty() {
            ctx.set_fill_color(TEXT_MUTED);
            ctx.set_font("14px pt root ui");
            ctx.set_text_align(TextAlign::Center);
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.fill_text(
                "Серверы не настроены",
                w / 2.0,
                (list_top + add_btn_y) / 2.0,
            );
        } else {
            let chain_count = cfg.chains.len();
            let list_h = item_h * chain_count as f64;

            // List container background
            ctx.set_fill_color(SURFACE);
            ctx.fill_rounded_rect(margin, list_top, content_w, list_h, 16.0);
            ctx.set_stroke_color(SURFACE_2);
            ctx.set_stroke_width(1.0);
            ctx.stroke_rounded_rect(margin, list_top, content_w, list_h, 16.0);

            // Snapshot chain data to avoid borrowing state twice
            let chain_data: Vec<(String, ProtocolId, String, String, bool)> = cfg
                .chains
                .iter()
                .enumerate()
                .map(|(idx, ch)| {
                    // Region/SNI label: prefer l2.sni as region hint, fall back to addr
                    let region = if ch.l2.sni.is_empty() {
                        ch.l2.addr.ip().to_string()
                    } else {
                        ch.l2.sni.clone()
                    };
                    let l1_hop = if ch.l1.sni.is_empty() {
                        ch.l1.addr.ip().to_string()
                    } else {
                        ch.l1.sni.clone()
                    };
                    (
                        ch.chain_id.clone(),
                        ch.protocol,
                        region,
                        l1_hop,
                        idx == state.selected_chain,
                    )
                })
                .collect();

            let mut clicked_select: Option<usize> = None;
            let mut clicked_settings: Option<usize> = None;

            for (idx, (chain_id, protocol, region, l1_hop, is_selected)) in
                chain_data.iter().enumerate()
            {
                let item_y = list_top + idx as f64 * item_h;
                let status = DisplayStatus::for_index(idx);

                // ---- Row hit area (excluding gear) ----
                let row_rect = Rect::new(margin, item_y, content_w - 56.0, item_h);
                let row_id = WidgetId::new(format!("server_row_{idx}"));
                state.coordinator.register(row_id.clone(), row_rect, Sense::CLICK);
                let row_hovered = state.coordinator.is_hovered(&row_id);

                if row_hovered {
                    ctx.set_fill_color_alpha(SURFACE_2, 0.4);
                    ctx.fill_rect(margin, item_y, content_w - 56.0, item_h);
                }

                // ---- Radio indicator ----
                let radio_cx = margin + 28.0;
                let radio_cy = item_y + item_h / 2.0;
                let radio_r = 9.0;

                if *is_selected {
                    ctx.begin_path();
                    ctx.arc(radio_cx, radio_cy, radio_r, 0.0, std::f64::consts::PI * 2.0);
                    ctx.set_stroke_color(ACCENT);
                    ctx.set_stroke_width(2.0);
                    ctx.stroke();
                    ctx.begin_path();
                    ctx.arc(radio_cx, radio_cy, radio_r * 0.5, 0.0, std::f64::consts::PI * 2.0);
                    ctx.set_fill_color(ACCENT);
                    ctx.fill();
                } else {
                    ctx.begin_path();
                    ctx.arc(radio_cx, radio_cy, radio_r, 0.0, std::f64::consts::PI * 2.0);
                    ctx.set_stroke_color(SEPARATOR);
                    ctx.set_stroke_width(2.0);
                    ctx.stroke();
                }

                // ---- Text column ----
                let text_x = margin + 52.0;
                let name_y = item_y + 16.0;

                // Chain name (bold, top line)
                let name_color = if *is_selected { ACCENT } else { TEXT_PRIMARY };
                ctx.set_fill_color(name_color);
                ctx.set_font("bold 16px pt root ui");
                ctx.set_text_align(TextAlign::Left);
                ctx.set_text_baseline(TextBaseline::Top);
                ctx.fill_text(chain_id, text_x, name_y);

                // Protocol badge (right of chain name, same line)
                let proto_label = protocol_display_name(*protocol);
                let badge_x = text_x + chain_id.len() as f64 * 9.0 + 8.0; // rough name width
                draw_protocol_badge(ctx, proto_label, badge_x, name_y - 1.0);

                // Region / hop info (second line)
                let hop_text = format!("{l1_hop}  →  {region}");
                ctx.set_fill_color(TEXT_MUTED);
                ctx.set_font("11px pt root ui");
                ctx.set_text_baseline(TextBaseline::Top);
                ctx.fill_text(&hop_text, text_x, name_y + 24.0);

                // Status dot + label (third line)
                let status_row_y = name_y + 44.0;
                let dot_cx = text_x + 6.0;
                let dot_cy = status_row_y + 6.0;
                draw_status_dot(ctx, dot_cx, dot_cy, status);

                ctx.set_fill_color(status.color());
                ctx.set_font("11px pt root ui");
                ctx.set_text_align(TextAlign::Left);
                ctx.set_text_baseline(TextBaseline::Top);
                ctx.fill_text(status.label(), text_x + 16.0, status_row_y);

                // ---- Settings gear button (right side) ----
                let gear_size = 40.0;
                let gear_x = margin + content_w - gear_size - 8.0;
                let gear_y = item_y + (item_h - gear_size) / 2.0;
                let gear_rect = Rect::new(gear_x, gear_y, gear_size, gear_size);
                let gear_id = WidgetId::new(format!("server_gear_{idx}"));
                state.coordinator.register(gear_id.clone(), gear_rect, Sense::CLICK);
                let gear_hovered = state.coordinator.is_hovered(&gear_id);

                if gear_hovered {
                    ctx.set_fill_color_alpha(SURFACE_2, 0.6);
                    ctx.fill_rounded_rect(gear_x, gear_y, gear_size, gear_size, 8.0);
                }

                let icon_sz = 22.0;
                draw_svg_icon(
                    ctx,
                    ICON_SETTINGS,
                    gear_x + (gear_size - icon_sz) / 2.0,
                    gear_y + (gear_size - icon_sz) / 2.0,
                    icon_sz,
                    icon_sz,
                    TEXT_MUTED,
                );

                // Divider between rows
                if idx < chain_data.len().saturating_sub(1) {
                    draw_divider(ctx, margin + 52.0, item_y + item_h, content_w - 52.0 - 8.0);
                }

                if clicked && row_hovered {
                    clicked_select = Some(idx);
                }
                if clicked && gear_hovered {
                    clicked_settings = Some(idx);
                }
            }

            // Handle clicks after the iteration to avoid re-borrow
            if let Some(idx) = clicked_select {
                state.select_chain(idx);
            }
            if let Some(idx) = clicked_settings {
                state.push_screen(Screen::ServerInfo { server_idx: idx });
            }

            // Suppress unused import
            let _ = badge_width;
        }
    } else {
        // No config loaded
        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("14px pt root ui");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(
            "Конфигурация не загружена",
            w / 2.0,
            (list_top + add_btn_y) / 2.0,
        );

        // Still suppress badge_width when no config
        let _ = badge_width;
    }

    // -------------------------------------------------------------------------
    // "Добавить сервер" button at bottom
    // -------------------------------------------------------------------------
    let add_btn_w = content_w;
    let add_btn_x = margin;

    let add_id = WidgetId::new("servers_add_btn");
    let add_rect = Rect::new(add_btn_x, add_btn_y, add_btn_w, add_btn_h);
    state.coordinator.register(add_id.clone(), add_rect, Sense::CLICK);
    let add_hovered = state.coordinator.is_hovered(&add_id);

    ctx.set_fill_color(if add_hovered { SURFACE_2 } else { SURFACE });
    ctx.fill_rounded_rect(add_btn_x, add_btn_y, add_btn_w, add_btn_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(add_btn_x, add_btn_y, add_btn_w, add_btn_h, 12.0);

    let plus_sz = 20.0;
    draw_svg_icon(
        ctx,
        ICON_PLUS,
        add_btn_x + 16.0,
        add_btn_y + (add_btn_h - plus_sz) / 2.0,
        plus_sz,
        plus_sz,
        ACCENT,
    );
    ctx.set_fill_color(ACCENT);
    ctx.set_font("bold 15px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.fill_text(
        "Добавить сервер",
        add_btn_x + 16.0 + plus_sz + 10.0,
        add_btn_y + add_btn_h / 2.0,
    );

    if clicked && add_hovered {
        state.go_to_tab(3); // Connect tab
    }

    // -------------------------------------------------------------------------
    // Nav bar
    // -------------------------------------------------------------------------
    if let Some(tab) = draw_nav_bar(ctx, input, state, w, h, 1) {
        state.go_to_tab(tab);
    }
}
