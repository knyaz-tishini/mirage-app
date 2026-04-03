//! Protocol detail screen — shown when tapping a protocol row in ServerInfo.
//!
//! `protocol_idx` maps to the 5 Mirage protocols in display order:
//! 0 = Mirage, 1 = VlessReality, 2 = Xhttp, 3 = ShadowTls, 4 = Hysteria2

use mirage_core::config::ProtocolId;
use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::input::InputState;
use uzor::types::Rect;

use crate::app::AppState;
use super::components::{
    BG, SURFACE, SURFACE_2, ACCENT, TEXT_PRIMARY, TEXT_MUTED,
    draw_back_header, draw_divider,
};

/// Draw the protocol detail screen.
pub fn draw(
    ctx: &mut dyn RenderContext,
    input: &InputState,
    viewport: Rect,
    state: &mut AppState,
    chain_idx: usize,
    protocol_idx: usize,
) {
    let w = viewport.width;
    let h = viewport.height;

    ctx.set_fill_color(BG);
    ctx.fill_rect(0.0, 0.0, w, h);

    let protocol_id = protocol_idx_to_id(protocol_idx);
    let proto_name = protocol_display_name(protocol_id);

    if draw_back_header(ctx, input, state, proto_name, w) {
        state.pop_screen();
        return;
    }

    let margin = 16.0;
    let content_w = w - margin * 2.0;
    let mut y = 80.0;

    // Protocol name + description header
    ctx.set_fill_color(TEXT_PRIMARY);
    ctx.set_font("bold 22px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(proto_name, margin, y);
    y += 32.0;

    let proto_desc = protocol_description(protocol_id);
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("13px pt root ui");
    ctx.fill_text(proto_desc, margin, y);
    y += 28.0;

    draw_divider(ctx, margin, y, content_w);
    y += 16.0;

    // Protocol-specific detail fields
    match protocol_id {
        ProtocolId::Mirage => draw_mirage_detail(ctx, state, margin, y, content_w, chain_idx),
        ProtocolId::VlessReality => draw_vless_detail(ctx, state, margin, y, content_w, chain_idx),
        ProtocolId::Xhttp => draw_xhttp_detail(ctx, state, margin, y, content_w, chain_idx),
        ProtocolId::ShadowTls => draw_shadowtls_detail(ctx, state, margin, y, content_w),
        ProtocolId::Hysteria2 => draw_hysteria2_detail(ctx, state, margin, y, content_w),
    }

    let _ = h;
}

// ---------------------------------------------------------------------------
// Per-protocol detail drawers
// ---------------------------------------------------------------------------

fn draw_mirage_detail(
    ctx: &mut dyn RenderContext,
    _state: &mut AppState,
    margin: f64,
    mut y: f64,
    content_w: f64,
    chain_idx: usize,
) {
    // Camouflage type selector (read-only display)
    let camouflage_types = ["JsonApi", "BinaryBlob", "GrpcFrame", "WebSocket", "Raw"];
    let camouflage_descs = [
        "Похоже на JSON API ответ",
        "Непрозрачный бинарный поток",
        "Похоже на gRPC фрейм",
        "Похоже на WebSocket",
        "Без маскировки",
    ];

    draw_section_header(ctx, "Тип маскировки трафика", margin, y);
    y += 28.0;

    let item_h = 48.0;
    let list_h = item_h * camouflage_types.len() as f64;
    ctx.set_fill_color(super::components::SURFACE);
    ctx.fill_rounded_rect(margin, y, content_w, list_h, 12.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(margin, y, content_w, list_h, 12.0);

    // Active camouflage — use chain_idx as seed to pick one for demo
    let active_idx = chain_idx % camouflage_types.len();

    for (i, (name, desc)) in camouflage_types.iter().zip(camouflage_descs.iter()).enumerate() {
        let item_y = y + i as f64 * item_h;
        let is_active = i == active_idx;

        if is_active {
            ctx.set_fill_color_alpha(ACCENT, 0.1);
            ctx.fill_rect(margin, item_y, content_w, item_h);
        }

        // Active dot
        if is_active {
            ctx.set_fill_color(ACCENT);
            ctx.begin_path();
            ctx.arc(margin + 18.0, item_y + item_h / 2.0, 5.0, 0.0, std::f64::consts::TAU);
            ctx.fill();
        } else {
            ctx.set_stroke_color(SURFACE_2);
            ctx.set_stroke_width(1.5);
            ctx.begin_path();
            ctx.arc(margin + 18.0, item_y + item_h / 2.0, 5.0, 0.0, std::f64::consts::TAU);
            ctx.stroke();
        }

        let text_color = if is_active { TEXT_PRIMARY } else { TEXT_MUTED };
        ctx.set_fill_color(text_color);
        ctx.set_font("bold 14px pt root ui");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Bottom);
        ctx.fill_text(name, margin + 32.0, item_y + item_h / 2.0 - 1.0);

        ctx.set_fill_color(TEXT_MUTED);
        ctx.set_font("11px pt root ui");
        ctx.set_text_baseline(TextBaseline::Top);
        ctx.fill_text(desc, margin + 32.0, item_y + item_h / 2.0 + 2.0);

        if i < camouflage_types.len().saturating_sub(1) {
            draw_divider(ctx, margin + 12.0, item_y + item_h, content_w - 12.0);
        }
    }

    y += list_h + 16.0;

    // Chunk size display
    draw_section_header(ctx, "Размер чанка", margin, y);
    y += 28.0;
    draw_info_row(ctx, "Размер пакета", "4096 байт", margin, y, content_w);
    y += 60.0;
    draw_info_note(ctx, "Чанки 4096 / 8192 / 16384 байт устойчивы к анализу трафика.", margin, y);
}

fn draw_vless_detail(
    ctx: &mut dyn RenderContext,
    state: &mut AppState,
    margin: f64,
    mut y: f64,
    content_w: f64,
    chain_idx: usize,
) {
    draw_section_header(ctx, "Reality TLS", margin, y);
    y += 28.0;

    let sni = state
        .config
        .as_ref()
        .and_then(|c| c.chains.get(chain_idx))
        .map(|ch| ch.l2.sni.clone())
        .unwrap_or_else(|| "—".to_string());

    draw_info_row(ctx, "server_name (SNI)", &sni, margin, y, content_w);
    y += 68.0;

    // Short IDs — no reality config on client ChainConfig, so placeholder
    draw_info_row(ctx, "short_ids", "Задаются на сервере", margin, y, content_w);
    y += 68.0;

    draw_info_row(ctx, "max_time_diff", "Задаётся на сервере (секунды)", margin, y, content_w);
    y += 68.0;

    draw_info_note(ctx, "Reality маскирует трафик под настоящий TLS-хендшейк.", margin, y);
}

fn draw_xhttp_detail(
    ctx: &mut dyn RenderContext,
    state: &mut AppState,
    margin: f64,
    mut y: f64,
    content_w: f64,
    chain_idx: usize,
) {
    draw_section_header(ctx, "XHTTP транспорт", margin, y);
    y += 28.0;

    let sni = state
        .config
        .as_ref()
        .and_then(|c| c.chains.get(chain_idx))
        .map(|ch| ch.l2.sni.clone())
        .unwrap_or_else(|| "—".to_string());

    draw_info_row(ctx, "Транспорт", "XHTTP (XRay)", margin, y, content_w);
    y += 68.0;

    draw_info_row(ctx, "SNI", &sni, margin, y, content_w);
    y += 68.0;

    draw_info_row(ctx, "Метод", "POST / chunked streaming", margin, y, content_w);
    y += 68.0;

    draw_info_note(ctx, "XHTTP туннелирует данные через обычные HTTP-запросы.", margin, y);
}

fn draw_shadowtls_detail(
    ctx: &mut dyn RenderContext,
    _state: &mut AppState,
    margin: f64,
    mut y: f64,
    content_w: f64,
) {
    draw_section_header(ctx, "ShadowTLS", margin, y);
    y += 28.0;

    draw_info_row_masked(ctx, "PSK", "••••••••••••••••", margin, y, content_w);
    y += 68.0;

    draw_info_row(ctx, "Версия", "ShadowTLS v3", margin, y, content_w);
    y += 68.0;

    draw_info_row(ctx, "Маскировка", "Настоящий TLS-хендшейк с реальным сайтом", margin, y, content_w);
    y += 68.0;

    draw_info_note(ctx, "PSK (pre-shared key) задаётся в конфигурации узла. В приложении не редактируется.", margin, y);
}

fn draw_hysteria2_detail(
    ctx: &mut dyn RenderContext,
    _state: &mut AppState,
    margin: f64,
    mut y: f64,
    content_w: f64,
) {
    draw_section_header(ctx, "Hysteria2", margin, y);
    y += 28.0;

    draw_info_row_masked(ctx, "PSK", "••••••••••••••••", margin, y, content_w);
    y += 68.0;

    draw_info_row(ctx, "Транспорт", "QUIC (UDP)", margin, y, content_w);
    y += 68.0;

    draw_info_row(ctx, "Пропускная способность", "Задаётся на сервере", margin, y, content_w);
    y += 68.0;

    draw_info_note(ctx, "Hysteria2 работает поверх QUIC. Требует открытого UDP-порта.", margin, y);
}

// ---------------------------------------------------------------------------
// Drawing helpers
// ---------------------------------------------------------------------------

fn draw_section_header(ctx: &mut dyn RenderContext, title: &str, x: f64, y: f64) {
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(title.to_uppercase().as_str(), x, y);
}

fn draw_info_row(
    ctx: &mut dyn RenderContext,
    label: &str,
    value: &str,
    x: f64,
    y: f64,
    w: f64,
) {
    draw_info_row_inner(ctx, label, value, false, x, y, w);
}

fn draw_info_row_masked(
    ctx: &mut dyn RenderContext,
    label: &str,
    value: &str,
    x: f64,
    y: f64,
    w: f64,
) {
    draw_info_row_inner(ctx, label, value, true, x, y, w);
}

fn draw_info_row_inner(
    ctx: &mut dyn RenderContext,
    label: &str,
    value: &str,
    masked: bool,
    x: f64,
    y: f64,
    w: f64,
) {
    let row_h = 56.0;
    ctx.set_fill_color(SURFACE);
    ctx.fill_rounded_rect(x, y, w, row_h, 10.0);
    ctx.set_stroke_color(SURFACE_2);
    ctx.set_stroke_width(1.0);
    ctx.stroke_rounded_rect(x, y, w, row_h, 10.0);

    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("11px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(label, x + 14.0, y + 8.0);

    let value_color = if masked { TEXT_MUTED } else { TEXT_PRIMARY };
    ctx.set_fill_color(value_color);
    ctx.set_font("14px pt root ui");
    ctx.set_text_baseline(TextBaseline::Bottom);
    ctx.fill_text(value, x + 14.0, y + row_h - 8.0);
}

fn draw_info_note(ctx: &mut dyn RenderContext, note: &str, x: f64, y: f64) {
    ctx.set_fill_color(TEXT_MUTED);
    ctx.set_font("12px pt root ui");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Top);
    ctx.fill_text(note, x, y);
}

// ---------------------------------------------------------------------------
// Protocol index → id mapping (matches display order in server_info_screen)
// ---------------------------------------------------------------------------

fn protocol_idx_to_id(idx: usize) -> ProtocolId {
    match idx {
        0 => ProtocolId::Mirage,
        1 => ProtocolId::VlessReality,
        2 => ProtocolId::Xhttp,
        3 => ProtocolId::ShadowTls,
        _ => ProtocolId::Hysteria2,
    }
}

fn protocol_display_name(id: ProtocolId) -> &'static str {
    match id {
        ProtocolId::Mirage => "Mirage",
        ProtocolId::VlessReality => "VLESS+Reality",
        ProtocolId::Xhttp => "XHTTP",
        ProtocolId::ShadowTls => "ShadowTLS",
        ProtocolId::Hysteria2 => "Hysteria2",
    }
}

fn protocol_description(id: ProtocolId) -> &'static str {
    match id {
        ProtocolId::Mirage => "Нативный протокол с маскировкой трафика (ChaCha20-Poly1305)",
        ProtocolId::VlessReality => "XRay VLESS с Reality TLS-маскировкой",
        ProtocolId::Xhttp => "XRay транспорт XHTTP поверх HTTP",
        ProtocolId::ShadowTls => "ShadowTLS — скрытие трафика под настоящий TLS",
        ProtocolId::Hysteria2 => "Hysteria2 — быстрый VPN-протокол поверх QUIC",
    }
}
