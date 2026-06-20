//! BloomSkin::render — pure vector bloom via RenderContext.
//!
//! Ported from `drawBloom()` in `mirage-desktop-client.html`.
//! All drawing uses the `RenderContext` trait's path primitives
//! (move_to / quadratic_curve_to / arc / fill / stroke / fill_rect).
//! No pixmap upload in this slice — rasterizer modes are deferred.

use std::f64::consts::TAU;
use uzor::render::RenderContext;

use super::state::{
    ihash, lerp_color, tint_color, BloomState, CORE_C, DEEP, INNER, OUTER, RINGS, SEED,
};
use crate::centerpiece::{CenterpieceState, SkinRect};

/// Draw one petal.
///
/// Matches JS `petal(bctx, cx, cy, a, rb, L, w, col, h)`.
fn draw_petal(
    ctx: &mut dyn RenderContext,
    cx: f64,
    cy: f64,
    angle: f64,
    rb: f64,
    length: f64,
    width: f64,
    col: [u8; 3],
    h: f64,
) {
    let v = 0.88 + h * 0.24;
    let r = ((col[0] as f64 * v) as u8).min(255);
    let g = ((col[1] as f64 * v) as u8).min(255);
    let b = ((col[2] as f64 * v) as u8).min(255);

    let color_str = format!("#{:02X}{:02X}{:02X}", r, g, b);

    // Base point on inner ring.
    let bx2 = cx + angle.cos() * rb;
    let by2 = cy + angle.sin() * rb;
    // Tip.
    let tx = cx + angle.cos() * (rb + length);
    let ty = cy + angle.sin() * (rb + length);
    // Control points (left / right quadratic handles).
    let c1x = cx + (angle - width).cos() * (rb + length * 0.62);
    let c1y = cy + (angle - width).sin() * (rb + length * 0.62);
    let c2x = cx + (angle + width).cos() * (rb + length * 0.62);
    let c2y = cy + (angle + width).sin() * (rb + length * 0.62);

    ctx.set_fill_color(&color_str);
    ctx.begin_path();
    ctx.move_to(bx2, by2);
    ctx.quadratic_curve_to(c1x, c1y, tx, ty);
    ctx.quadratic_curve_to(c2x, c2y, bx2, by2);
    ctx.close_path();
    ctx.fill();
}

/// Draw the dot-grid background shared across all skins.
pub fn draw_dot_grid(ctx: &mut dyn RenderContext, rect: &SkinRect) {
    const STEP: f64 = 24.0;
    ctx.set_fill_color("rgba(255,255,255,0.04)");
    let mut yy = rect.y + 8.0;
    while yy < rect.y + rect.h {
        let mut xx = rect.x + 8.0;
        while xx < rect.x + rect.w {
            ctx.fill_rect(xx, yy, 1.0, 1.0);
            xx += STEP;
        }
        yy += STEP;
    }
}

/// Draw the full bloom scene (vector-only).
///
/// `hx`, `hy` — bloom head centre in logical pixels.
/// `head_r`   — radius of the bloom head (scales with window size).
/// `st`       — current eased state.
pub fn draw_bloom(
    ctx: &mut dyn RenderContext,
    hx: f64,
    hy: f64,
    head_r: f64,
    st: &BloomState,
) {
    let t = st.t;
    let glow_i = st.glow_i;
    let stem_t = st.stem_t;
    let trem = st.trem;

    // ── Jitter ────────────────────────────────────────────────────────────────
    let hx = hx + 2.5 * trem * (t * 3.0).sin();

    // ── Radial glow (layered concentric arcs — fallback for radial gradient) ──
    // JS uses createRadialGradient; we approximate with 4 concentric arcs at
    // decreasing alpha.  This matches the design doc's stated fallback.
    if glow_i > 0.02 {
        let gr = head_r * 1.28;
        let pulse = 1.0 + 0.08 * (t * 2.5).sin();
        let gr_p = gr * pulse;

        // 4 rings from inner to outer with decreasing alpha.
        let stops: [(f64, f64); 4] = [
            (0.0,  0.6 * glow_i),
            (0.33, 0.30 * glow_i),
            (0.66, 0.12 * glow_i),
            (1.0,  0.0),
        ];
        for i in 0..4 {
            let t0 = stops[i].0;
            let t1 = if i + 1 < 4 { stops[i + 1].0 } else { 1.0 };
            let a0 = stops[i].1;
            let a1 = if i + 1 < 4 { stops[i + 1].1 } else { 0.0 };
            let alpha = (a0 + a1) * 0.5;
            if alpha < 0.01 {
                continue;
            }
            let r_inner = head_r * 0.3 + (gr_p - head_r * 0.3) * t0;
            let r_outer = head_r * 0.3 + (gr_p - head_r * 0.3) * t1;
            let mid_r = (r_inner + r_outer) * 0.5;
            let alpha_byte = (alpha * 255.0).clamp(0.0, 255.0) as u8;
            let color = format!("rgba(255,216,152,{:.2})", alpha_byte as f64 / 255.0);
            ctx.set_fill_color(&color);
            ctx.set_global_alpha(1.0);
            ctx.begin_path();
            ctx.arc(hx, hy, mid_r, 0.0, TAU);
            ctx.fill();
        }
        ctx.set_global_alpha(1.0);
    }

    // ── Stem ─────────────────────────────────────────────────────────────────
    let stem_base_y = hy + head_r * (180.0 / 46.0); // proportional to head_r
    if stem_t > 0.05 {
        ctx.set_stroke_color("rgb(70,90,46)");
        ctx.set_stroke_width(2.8);
        ctx.set_global_alpha((stem_t * 1.6).min(1.0));
        ctx.begin_path();
        ctx.move_to(hx, stem_base_y);
        ctx.quadratic_curve_to(hx + 3.0, (stem_base_y + hy) / 2.0, hx, hy + 10.0);
        ctx.stroke();
        ctx.set_global_alpha(1.0);
    }

    // ── Petals (6 rings) ─────────────────────────────────────────────────────
    let spread_r = st.spd.to_radians();
    let len_f = 1.1 - 0.1 * st.e_norm;

    for k in 0..RINGS {
        let kt = k as f64 / (RINGS as f64 - 1.0);
        let count = 10 - k; // rings: 10, 9, 8, 7, 6, 5 petals
        let base_col = if k < RINGS - 2 {
            lerp_color(OUTER, INNER, kt)
        } else {
            lerp_color(INNER, DEEP, kt)
        };
        let col = tint_color(base_col, st.tod_light);
        let petal_l = head_r * (1.0 - kt * 0.45) * len_f;
        let rb = 2.0 + head_r * 0.08 * (1.0 - kt);
        let w = ((spread_r / count as f64) * 0.62).clamp(0.16, 0.7);
        let sway = 0.08
            * (t * 1.1 + k as f64 * 1.3).sin()
            * if k % 2 == 1 { 1.0 } else { -1.0 };

        for i in 0..count {
            let h = ihash(k as u32, i as u32, SEED);
            let angle = -std::f64::consts::FRAC_PI_2
                + ((i as f64 + 0.5) / count as f64 - 0.5) * spread_r
                + sway
                + (h - 0.5) * 0.10
                + 0.03 * (t * 1.6 + i as f64 + k as f64).sin();
            let lb = petal_l
                * (0.85 + h * 0.3)
                * (1.0 + 0.05 * (t * 1.4 + i as f64 * 0.6 + k as f64).sin());

            draw_petal(ctx, hx, hy, angle, rb, lb, w, col, h);
        }
    }

    // ── Core circle ───────────────────────────────────────────────────────────
    let core_color = format!("rgb({},{},{})", CORE_C[0], CORE_C[1], CORE_C[2]);
    ctx.set_fill_color(&core_color);
    ctx.begin_path();
    ctx.arc(hx, hy, 5.0 + 0.6 * (t * 2.2).sin(), 0.0, TAU);
    ctx.fill();

    // ── Sparkles ─────────────────────────────────────────────────────────────
    if glow_i > 0.05 {
        let n_sp = (5.0 + 10.0 * glow_i) as usize;
        let gr2 = head_r * 1.2;
        // Deterministic sparkles using ihash (avoids rand dependency).
        for i in 0..n_sp {
            let h1 = ihash(i as u32, 77, SEED);
            let h2 = ihash(i as u32, 88, SEED);
            let h3 = ihash(i as u32, 99, SEED);
            let angle = h1 * TAU;
            let rr = gr2 * 0.45 + h2 * gr2 * 0.7;
            let px = hx + angle.cos() * rr;
            let py = hy + angle.sin() * rr;
            let al = h3 * glow_i * 0.85;
            let alpha_str = format!("rgba(255,236,200,{:.2})", al);
            ctx.set_fill_color(&alpha_str);
            ctx.fill_rect(px, py, 1.3, 1.3);
        }
    }
}

/// Render the full centerpiece: background + bloom centred in `rect`.
pub fn render_bloom(ctx: &mut dyn RenderContext, st: &BloomState, frame: CenterpieceState) {
    let rect = frame.rect;

    // Background fill.
    ctx.set_fill_color("#0B0B0E");
    ctx.fill_rect(rect.x, rect.y, rect.w, rect.h);

    // Dot-grid.
    draw_dot_grid(ctx, &rect);

    // Bloom centred in the available rect.
    let hx = rect.cx();
    // Bloom head sits slightly above centre (matching JS `hy = 108 - 24*stemT`
    // in a 180px-tall buffer, i.e. roughly 40% from top).
    let head_r = rect.w.min(rect.h) * 0.22;
    let hy = rect.y + rect.h * 0.42 - head_r * 0.5 * st.stem_t;

    draw_bloom(ctx, hx, hy, head_r, st);
}
