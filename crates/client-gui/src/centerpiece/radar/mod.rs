//! RadarSkin — concentric-ring sweep radar with fading arm and ambient blips.
//!
//! Disconnected: 3 rings, slower sweep.
//! Connected:    5 rings, faster sweep, brighter blips.
//!
//! Inspired by the "Meridian Radar" feel from zeng3ld-landing inst-signals.js
//! but implemented as a simple first-cut using RenderContext primitives.

use std::f64::consts::TAU;

use uzor::render::RenderContext;

use crate::centerpiece::{CenterpieceState, ConnPhase, Skin, SkinRect};

// ── Blip ──────────────────────────────────────────────────────────────────────

/// A deterministic ambient blip dot on the radar.
#[derive(Clone, Copy)]
struct Blip {
    /// Angle in radians (static — blip doesn't orbit).
    angle: f64,
    /// Normalized radius [0, 1] relative to the outermost ring.
    r_norm: f64,
    /// Brightness [0, 1] for variation.
    brightness: f64,
}

/// Build a fixed set of ambient blips from a deterministic seed.
fn make_blips(n: usize, seed: u32) -> Vec<Blip> {
    (0..n)
        .map(|i| {
            let h1 = ihash(i as u32, 1, seed);
            let h2 = ihash(i as u32, 2, seed);
            let h3 = ihash(i as u32, 3, seed);
            Blip {
                angle: h1 * TAU,
                r_norm: 0.25 + h2 * 0.70,
                brightness: 0.35 + h3 * 0.65,
            }
        })
        .collect()
}

/// Simple integer hash → [0, 1).
fn ihash(i: u32, j: u32, seed: u32) -> f64 {
    let h = (i.wrapping_mul(374_761_393))
        .wrapping_add(j.wrapping_mul(668_265_263))
        .wrapping_add(seed);
    let h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    let h = h ^ (h >> 16);
    h as f64 / 4_294_967_296.0
}

// ── RadarState ────────────────────────────────────────────────────────────────

struct RadarState {
    /// Accumulated time (seconds).
    t: f64,
    /// Sweep arm angle (radians).
    arm: f64,
    /// Eased sweep speed (rad/s).
    speed: f64,
    /// Eased ring count target (3 disconnected, 5 connected).
    ring_t: f64,
    /// Eased global brightness.
    brightness: f64,
}

impl RadarState {
    fn new() -> Self {
        Self {
            t: 0.0,
            arm: 0.0,
            speed: 0.45,
            ring_t: 0.0,
            brightness: 0.6,
        }
    }

    fn tick(&mut self, dt: f64, phase: ConnPhase) {
        self.t += dt;

        let connected = matches!(phase, ConnPhase::Connected | ConnPhase::Disconnecting);

        let tgt_speed      = if connected { 1.1 } else { 0.45 };
        let tgt_ring_t     = if connected { 1.0 } else { 0.0 };
        let tgt_brightness = if connected { 1.0 } else { 0.6 };

        let ease = |x: f64, tgt: f64, r: f64| x + (tgt - x) * (r * dt).min(1.0);

        self.speed      = ease(self.speed, tgt_speed, 2.0);
        self.ring_t     = ease(self.ring_t, tgt_ring_t, 2.0);
        self.brightness = ease(self.brightness, tgt_brightness, 2.5);
        self.arm        += self.speed * dt;
        if self.arm >= TAU {
            self.arm -= TAU;
        }
    }

    /// Interpolated ring count: 3 (disconnected) to 5 (connected).
    fn ring_count(&self) -> f64 {
        3.0 + self.ring_t * 2.0
    }
}

// ── RadarSkin ─────────────────────────────────────────────────────────────────

/// Concentric-ring sweep radar skin.
pub struct RadarSkin {
    state: RadarState,
    blips: Vec<Blip>,
}

impl RadarSkin {
    pub fn new() -> Self {
        Self {
            state: RadarState::new(),
            blips: make_blips(14, 112_233),
        }
    }
}

impl Default for RadarSkin {
    fn default() -> Self {
        Self::new()
    }
}

impl Skin for RadarSkin {
    fn render(&mut self, ctx: &mut dyn RenderContext, st: CenterpieceState) {
        self.state.tick(st.dt, st.phase);
        render_radar(ctx, &self.state, &self.blips, st);
    }
}

// ── Render ────────────────────────────────────────────────────────────────────

fn render_radar(
    ctx: &mut dyn RenderContext,
    state: &RadarState,
    blips: &[Blip],
    frame: CenterpieceState,
) {
    let rect = frame.rect;

    // Background.
    ctx.set_fill_color("#060C08");
    ctx.fill_rect(rect.x, rect.y, rect.w, rect.h);

    // Subtle dot grid.
    draw_dot_grid(ctx, &rect);

    let cx = rect.cx();
    let cy = rect.cy();
    let max_r = rect.w.min(rect.h) * 0.36;
    let br = state.brightness;
    let ring_count = state.ring_count();

    // ── Concentric rings ──────────────────────────────────────────────────────

    let rings_floor = ring_count.floor() as usize;
    for k in 1..=5_usize {
        // Interpolated opacity: full rings up to floor, partial for the
        // fractional ring, invisible above.
        let alpha = if k <= rings_floor {
            0.22 * br
        } else if k == rings_floor + 1 {
            (ring_count - ring_count.floor()) * 0.22 * br
        } else {
            0.0
        };
        if alpha < 0.01 {
            continue;
        }

        let ring_r = max_r * (k as f64 / 5.0);
        let color = format!("rgba(60,220,100,{:.3})", alpha);
        ctx.set_stroke_color(&color);
        ctx.set_stroke_width(0.8);
        ctx.begin_path();
        ctx.arc(cx, cy, ring_r, 0.0, TAU);
        ctx.stroke();
    }

    // ── Cross-hair lines ──────────────────────────────────────────────────────

    let ch_alpha = 0.10 * br;
    let ch_color = format!("rgba(60,220,100,{:.3})", ch_alpha);
    ctx.set_stroke_color(&ch_color);
    ctx.set_stroke_width(0.6);
    ctx.begin_path();
    ctx.move_to(cx - max_r, cy);
    ctx.line_to(cx + max_r, cy);
    ctx.stroke();
    ctx.begin_path();
    ctx.move_to(cx, cy - max_r);
    ctx.line_to(cx, cy + max_r);
    ctx.stroke();

    // ── Sweep arm (fading wedge) ──────────────────────────────────────────────
    //
    // Draw a wedge covering ~40° behind the arm, fading from bright (at arm)
    // to transparent.  We approximate with a filled arc using set_global_alpha.

    let arm = state.arm;
    let wedge_span = std::f64::consts::PI * 0.22; // ~40°
    let wedge_steps = 18_usize;

    for i in 0..wedge_steps {
        let frac = i as f64 / wedge_steps as f64; // 0 = bright tip, 1 = fade tail
        let a0 = arm - wedge_span * frac;
        let a1 = arm - wedge_span * (frac + 1.0 / wedge_steps as f64);
        let alpha = (1.0 - frac) * 0.55 * br;
        if alpha < 0.005 {
            continue;
        }

        let color = format!("rgba(60,240,110,{:.3})", alpha);
        ctx.set_fill_color(&color);
        ctx.begin_path();
        ctx.move_to(cx, cy);
        ctx.arc(cx, cy, max_r * 1.01, a0, a1);
        ctx.close_path();
        ctx.fill();
    }

    // Bright tip line along the arm.
    let tip_alpha = 0.8 * br;
    let tip_color = format!("rgba(120,255,160,{:.3})", tip_alpha);
    ctx.set_stroke_color(&tip_color);
    ctx.set_stroke_width(1.2);
    ctx.begin_path();
    ctx.move_to(cx, cy);
    ctx.line_to(cx + arm.cos() * max_r, cy + arm.sin() * max_r);
    ctx.stroke();

    // ── Ambient blips ─────────────────────────────────────────────────────────

    for blip in blips {
        let blip_r = max_r * blip.r_norm;
        let bx = cx + blip.angle.cos() * blip_r;
        let by = cy + blip.angle.sin() * blip_r;

        // Blip lights up when the sweep arm passes over it.
        let angle_delta = (arm - blip.angle).rem_euclid(TAU);
        let flash = if angle_delta < 0.25 {
            1.0 - angle_delta / 0.25
        } else {
            0.0
        };
        let pulse = (1.0 + 0.18 * (state.t * 2.0 + blip.angle * 3.0).sin()) * 0.5;
        let alpha = (blip.brightness * 0.20 + flash * 0.70 + pulse * 0.10) * br;
        let dot_r = 1.5 + flash * 2.5;

        let color = format!("rgba(80,255,140,{:.3})", alpha.clamp(0.0, 1.0));
        ctx.set_fill_color(&color);
        ctx.begin_path();
        ctx.arc(bx, by, dot_r, 0.0, TAU);
        ctx.fill();
    }

    // Center pivot dot.
    ctx.set_fill_color("rgba(100,255,150,0.70)");
    ctx.begin_path();
    ctx.arc(cx, cy, 2.5, 0.0, TAU);
    ctx.fill();
}

fn draw_dot_grid(ctx: &mut dyn RenderContext, rect: &SkinRect) {
    const STEP: f64 = 24.0;
    ctx.set_fill_color("rgba(255,255,255,0.020)");
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
