//! Bloom render — CPU software-raster of the peony into a low-res RGB buffer,
//! then re-rasterized to the window in one of three pixel modes
//! (ascii / halftone / pixel) selected by connection phase.
//!
//! Faithful port of `drawScene()` + `rasterize()` from the JS reference
//! `mirage/app-playground/concepts/bloom-variant.html`. uzor's `RenderContext`
//! is a vector API with no pixel read-back, so the flower is rasterized on the
//! CPU here (petal polygons, radial glow, stem, core, sparkles) and only the
//! final downsampled cells are emitted as `fill_rect` / `arc` / `fill_text`.

use std::f64::consts::TAU;
use uzor::render::{RenderContext, TextAlign, TextBaseline};

use super::state::{
    ihash, lerp_color, tint_color, BloomState, CORE_C, DEEP, INNER, OUTER, RINGS, SEED,
};
use crate::centerpiece::{CenterpieceState, ConnPhase, SkinRect};

// ── Offscreen buffer geometry (matches the JS BW/BH) ────────────────────────────
const BW: usize = 240;
const BH: usize = 180;
/// Near-black buffer background (#060507).
const BG: [f32; 3] = [6.0, 5.0, 7.0];
/// ASCII luminance ramp " .,:;ox%#@".
const RAMP: &[u8] = b" .,:;ox%#@";

// ── CPU RGB buffer ──────────────────────────────────────────────────────────────

struct Buf {
    px: Vec<[f32; 3]>,
}

impl Buf {
    fn new() -> Self {
        Self { px: vec![BG; BW * BH] }
    }

    fn clear(&mut self) {
        for p in self.px.iter_mut() {
            *p = BG;
        }
    }

    #[inline]
    fn blend(&mut self, x: i32, y: i32, c: [f32; 3], a: f32) {
        if x < 0 || y < 0 || x >= BW as i32 || y >= BH as i32 || a <= 0.0 {
            return;
        }
        let i = (y as usize) * BW + x as usize;
        let p = &mut self.px[i];
        let ia = 1.0 - a;
        p[0] = p[0] * ia + c[0] * a;
        p[1] = p[1] * ia + c[1] * a;
        p[2] = p[2] * ia + c[2] * a;
    }

    #[inline]
    fn set(&mut self, x: i32, y: i32, c: [f32; 3]) {
        self.blend(x, y, c, 1.0);
    }

    #[inline]
    fn sample(&self, x: usize, y: usize) -> [f32; 3] {
        self.px[y * BW + x]
    }
}

// ── CPU primitives ──────────────────────────────────────────────────────────────

fn fill_circle(buf: &mut Buf, cx: f64, cy: f64, r: f64, c: [f32; 3], a: f32) {
    if r <= 0.0 {
        return;
    }
    let r2 = r * r;
    let y0 = (cy - r).floor().max(0.0) as i32;
    let y1 = (cy + r).ceil().min((BH - 1) as f64) as i32;
    let x0 = (cx - r).floor().max(0.0) as i32;
    let x1 = (cx + r).ceil().min((BW - 1) as f64) as i32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f64 + 0.5 - cx;
            let dy = y as f64 + 0.5 - cy;
            if dx * dx + dy * dy <= r2 {
                buf.blend(x, y, c, a);
            }
        }
    }
}

/// Even-odd scanline polygon fill.
fn fill_poly(buf: &mut Buf, pts: &[(f64, f64)], c: [f32; 3]) {
    if pts.len() < 3 {
        return;
    }
    let (mut min_y, mut max_y) = (f64::MAX, f64::MIN);
    for p in pts {
        min_y = min_y.min(p.1);
        max_y = max_y.max(p.1);
    }
    let y0 = min_y.floor().max(0.0) as i32;
    let y1 = max_y.ceil().min((BH - 1) as f64) as i32;
    let n = pts.len();
    let mut xs: Vec<f64> = Vec::with_capacity(8);
    for y in y0..=y1 {
        let yc = y as f64 + 0.5;
        xs.clear();
        for i in 0..n {
            let (x1, ya) = pts[i];
            let (x2, yb) = pts[(i + 1) % n];
            if (ya <= yc && yb > yc) || (yb <= yc && ya > yc) {
                let t = (yc - ya) / (yb - ya);
                xs.push(x1 + (x2 - x1) * t);
            }
        }
        if xs.len() < 2 {
            continue;
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut i = 0;
        while i + 1 < xs.len() {
            let xa = xs[i].ceil().max(0.0);
            let xb = xs[i + 1].floor().min((BW - 1) as f64);
            let mut x = xa;
            while x <= xb {
                buf.set(x as i32, y, c);
                x += 1.0;
            }
            i += 2;
        }
    }
}

#[inline]
fn quad_pt(p0: (f64, f64), ctrl: (f64, f64), p1: (f64, f64), t: f64) -> (f64, f64) {
    let mt = 1.0 - t;
    let a = mt * mt;
    let b = 2.0 * mt * t;
    let cc = t * t;
    (a * p0.0 + b * ctrl.0 + cc * p1.0, a * p0.1 + b * ctrl.1 + cc * p1.1)
}

fn stroke_quad(buf: &mut Buf, p0: (f64, f64), ctrl: (f64, f64), p1: (f64, f64), width: f64, c: [f32; 3], a: f32) {
    let r = width * 0.5;
    let steps = 26;
    for k in 0..=steps {
        let t = k as f64 / steps as f64;
        let (x, y) = quad_pt(p0, ctrl, p1, t);
        fill_circle(buf, x, y, r, c, a);
    }
}

/// One petal — base on the inner ring, tip outward, two quadratic edges.
fn fill_petal(buf: &mut Buf, cx: f64, cy: f64, angle: f64, rb: f64, length: f64, width: f64, col: [u8; 3], h: f64) {
    let v = 0.88 + h * 0.24;
    let c = [
        (col[0] as f64 * v).min(255.0) as f32,
        (col[1] as f64 * v).min(255.0) as f32,
        (col[2] as f64 * v).min(255.0) as f32,
    ];
    let base = (cx + angle.cos() * rb, cy + angle.sin() * rb);
    let tip = (cx + angle.cos() * (rb + length), cy + angle.sin() * (rb + length));
    let c1 = (
        cx + (angle - width).cos() * (rb + length * 0.62),
        cy + (angle - width).sin() * (rb + length * 0.62),
    );
    let c2 = (
        cx + (angle + width).cos() * (rb + length * 0.62),
        cy + (angle + width).sin() * (rb + length * 0.62),
    );

    let seg = 9;
    let mut pts: Vec<(f64, f64)> = Vec::with_capacity(seg * 2 + 2);
    pts.push(base);
    for k in 1..=seg {
        pts.push(quad_pt(base, c1, tip, k as f64 / seg as f64));
    }
    for k in 1..=seg {
        pts.push(quad_pt(tip, c2, base, k as f64 / seg as f64));
    }
    fill_poly(buf, &pts, c);
}

/// Radial glow alpha at normalised radius `tt` (stops: 0.6 → 0.24 → 0).
#[inline]
fn glow_alpha(tt: f64, gi: f64) -> f64 {
    if tt < 0.55 {
        let u = tt / 0.55;
        (0.6 + (0.24 - 0.6) * u) * gi
    } else {
        let u = (tt - 0.55) / 0.45;
        (0.24 - 0.24 * u) * gi
    }
}

// ── Scene → buffer (port of drawScene) ──────────────────────────────────────────

fn draw_scene(buf: &mut Buf, st: &BloomState) {
    buf.clear();

    let t = st.t;
    let glow_i = st.glow_i;
    let stem_t = st.stem_t;
    let trem = st.trem;

    let hx = BW as f64 / 2.0 + 2.5 * trem * (t * 3.0).sin();
    let hy = 108.0 - 24.0 * stem_t;
    let head_r = 46.0_f64;
    let spread_r = st.spd.to_radians();
    let len_f = 1.1 - 0.1 * st.e_norm;

    // ── Glow (radial gradient) ──────────────────────────────────────────────────
    if glow_i > 0.02 {
        let pulse = 1.0 + 0.08 * (t * 2.5).sin();
        let gr = head_r * 1.28 * pulse;
        let glow_c = [255.0, 216.0, 152.0];
        let y0 = (hy - gr).floor().max(0.0) as i32;
        let y1 = (hy + gr).ceil().min((BH - 1) as f64) as i32;
        let x0 = (hx - gr).floor().max(0.0) as i32;
        let x1 = (hx + gr).ceil().min((BW - 1) as f64) as i32;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f64 + 0.5 - hx;
                let dy = y as f64 + 0.5 - hy;
                let d = (dx * dx + dy * dy).sqrt();
                if d < gr {
                    let a = glow_alpha(d / gr, glow_i);
                    buf.blend(x, y, glow_c, a as f32);
                }
            }
        }
    }

    // ── Stem ───────────────────────────────────────────────────────────────────
    if stem_t > 0.05 {
        let a = (stem_t * 1.6).min(1.0) as f32;
        stroke_quad(
            buf,
            (hx, 178.0),
            (hx + 3.0, (178.0 + hy) / 2.0),
            (hx, hy + 10.0),
            2.8,
            [70.0, 90.0, 46.0],
            a,
        );
    }

    // ── Petals (6 rings) ─────────────────────────────────────────────────────────
    for k in 0..RINGS {
        let kt = k as f64 / (RINGS as f64 - 1.0);
        let count = 10 - k;
        let base_col = if k < RINGS - 2 {
            lerp_color(OUTER, INNER, kt)
        } else {
            lerp_color(INNER, DEEP, kt)
        };
        let col = tint_color(base_col, st.tod_light);
        let petal_l = head_r * (1.0 - kt * 0.45) * len_f;
        let rb = 2.0 + head_r * 0.08 * (1.0 - kt);
        let w = ((spread_r / count as f64) * 0.62).clamp(0.16, 0.7);
        let sway = 0.08 * (t * 1.1 + k as f64 * 1.3).sin() * if k % 2 == 1 { 1.0 } else { -1.0 };

        for i in 0..count {
            let h = ihash(k as u32, i as u32, SEED);
            let angle = -std::f64::consts::FRAC_PI_2
                + ((i as f64 + 0.5) / count as f64 - 0.5) * spread_r
                + sway
                + (h - 0.5) * 0.10
                + 0.03 * (t * 1.6 + i as f64 + k as f64).sin();
            let lb = petal_l * (0.85 + h * 0.3) * (1.0 + 0.05 * (t * 1.4 + i as f64 * 0.6 + k as f64).sin());
            fill_petal(buf, hx, hy, angle, rb, lb, w, col, h);
        }
    }

    // ── Core ───────────────────────────────────────────────────────────────────
    fill_circle(
        buf,
        hx,
        hy,
        5.0 + 0.6 * (t * 2.2).sin(),
        [CORE_C[0] as f32, CORE_C[1] as f32, CORE_C[2] as f32],
        1.0,
    );

    // ── Sparkles (deterministic via ihash) ───────────────────────────────────────
    if glow_i > 0.05 {
        let n_sp = (5.0 + 10.0 * glow_i) as usize;
        let gr2 = head_r * 1.2;
        for i in 0..n_sp {
            let h1 = ihash(i as u32, 77, SEED);
            let h2 = ihash(i as u32, 88, SEED);
            let h3 = ihash(i as u32, 99, SEED);
            let angle = h1 * TAU;
            let rr = gr2 * 0.45 + h2 * gr2 * 0.7;
            let px = hx + angle.cos() * rr;
            let py = hy + angle.sin() * rr;
            let al = (h3 * glow_i * 0.85) as f32;
            buf.blend(px as i32, py as i32, [255.0, 236.0, 200.0], al);
            buf.blend(px as i32 + 1, py as i32, [255.0, 236.0, 200.0], al);
        }
    }
}

// ── Buffer → window (port of rasterize) ──────────────────────────────────────────

/// Rasterize mode, selected by connection phase.
#[derive(Clone, Copy)]
enum Mode {
    Ascii,
    Halftone,
    Pixel,
}

fn mode_for(phase: ConnPhase) -> Mode {
    match phase {
        ConnPhase::Disconnected => Mode::Ascii,
        ConnPhase::Connecting => Mode::Halftone,
        ConnPhase::Disconnecting => Mode::Halftone,
        ConnPhase::Connected => Mode::Pixel,
    }
}

#[inline]
fn rgb_str(c: [f32; 3]) -> String {
    format!(
        "rgb({},{},{})",
        c[0].clamp(0.0, 255.0) as u8,
        c[1].clamp(0.0, 255.0) as u8,
        c[2].clamp(0.0, 255.0) as u8
    )
}

fn rasterize(ctx: &mut dyn RenderContext, ox: f64, oy: f64, scale: f64, buf: &Buf, mode: Mode) {
    let step: usize = matches!(mode, Mode::Pixel).then_some(4).unwrap_or(5);
    let sx = scale;
    let sy = scale;

    ctx.set_global_alpha(1.0);
    if let Mode::Ascii = mode {
        let fs = (step as f64 * sy).ceil();
        ctx.set_font(&format!("{}px monospace", fs as i32));
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Top);
    }

    let mut y = 0usize;
    while y < BH {
        let mut x2 = 0usize;
        while x2 < BW {
            let c = buf.sample(x2, y);
            let lum = (c[0] * 0.299 + c[1] * 0.587 + c[2] * 0.114) / 255.0;
            if lum >= 0.03 {
                let px = ox + x2 as f64 * sx;
                let py = oy + y as f64 * sy;
                match mode {
                    Mode::Pixel => {
                        ctx.set_fill_color(&rgb_str(c));
                        ctx.fill_rect(px, py, step as f64 * sx - 0.6, step as f64 * sy - 0.6);
                    }
                    Mode::Halftone => {
                        ctx.set_fill_color(&rgb_str(c));
                        ctx.begin_path();
                        ctx.arc(
                            px + step as f64 / 2.0 * sx,
                            py + step as f64 / 2.0 * sy,
                            lum as f64 * step as f64 * sx * 0.62,
                            0.0,
                            TAU,
                        );
                        ctx.fill();
                    }
                    Mode::Ascii => {
                        let lit = [
                            (c[0] + 45.0).min(255.0),
                            (c[1] + 45.0).min(255.0),
                            (c[2] + 45.0).min(255.0),
                        ];
                        ctx.set_fill_color(&rgb_str(lit));
                        let idx = ((lum as f64 * RAMP.len() as f64) as usize).min(RAMP.len() - 1);
                        let ch = (RAMP[idx] as char).to_string();
                        ctx.fill_text(&ch, px, py);
                    }
                }
            }
            x2 += step;
        }
        y += step;
    }
}

// ── Public entry ─────────────────────────────────────────────────────────────────

thread_local! {
    static BUF: std::cell::RefCell<Buf> = std::cell::RefCell::new(Buf::new());
}

/// Render the full centerpiece: background + rasterized bloom centred in `rect`.
pub fn render_bloom(ctx: &mut dyn RenderContext, st: &BloomState, frame: CenterpieceState) {
    let rect: SkinRect = frame.rect;

    // Window background.
    ctx.set_fill_color("#0B0B0E");
    ctx.fill_rect(rect.x, rect.y, rect.w, rect.h);

    // Fit the 4:3 buffer into the rect, centred.
    let scale = (rect.w / BW as f64).min(rect.h / BH as f64);
    let ow = BW as f64 * scale;
    let oh = BH as f64 * scale;
    let ox = rect.x + (rect.w - ow) / 2.0;
    let oy = rect.y + (rect.h - oh) / 2.0;

    let mode = mode_for(frame.phase);
    BUF.with(|b| {
        let mut buf = b.borrow_mut();
        draw_scene(&mut buf, st);
        rasterize(ctx, ox, oy, scale, &buf, mode);
    });
}
