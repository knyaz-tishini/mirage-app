//! PlanetSkin — Fibonacci-sphere dotted globe, gently auto-rotating.
//!
//! N ≈ 720 points placed via the golden-angle Fibonacci lattice, projected
//! orthographically onto the screen. Points on the front hemisphere (z > 0)
//! are brighter; points on the back hemisphere are dimmer. The globe
//! auto-rotates around the Y axis; rotation speed and dot brightness respond
//! to ConnPhase.

use std::f64::consts::TAU;

use uzor::render::RenderContext;

use crate::centerpiece::{CenterpieceState, ConnPhase, Skin, SkinRect};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Number of Fibonacci sphere points.
const N_POINTS: usize = 720;

/// Golden angle in radians.
const GOLDEN_ANGLE: f64 = std::f64::consts::PI * (3.0 - 2.2360679774997896); // π(3−√5)

// ── Precomputed sphere point ───────────────────────────────────────────────────

/// A unit-sphere point in Cartesian coordinates.
#[derive(Clone, Copy)]
struct SpherePoint {
    x: f64,
    y: f64,
    z: f64,
}

/// Build the Fibonacci lattice once.
fn build_fibonacci_sphere() -> [SpherePoint; N_POINTS] {
    let mut pts = [SpherePoint { x: 0.0, y: 0.0, z: 0.0 }; N_POINTS];
    for (i, pt) in pts.iter_mut().enumerate() {
        let theta = (1.0 - 2.0 * (i as f64 + 0.5) / N_POINTS as f64).acos();
        let phi = GOLDEN_ANGLE * i as f64;
        pt.x = theta.sin() * phi.cos();
        pt.y = theta.cos();
        pt.z = theta.sin() * phi.sin();
    }
    pts
}

// ── PlanetState ───────────────────────────────────────────────────────────────

struct PlanetState {
    /// Accumulated time (seconds).
    t: f64,
    /// Yaw angle (radians) — advances each frame.
    yaw: f64,
    /// Eased brightness multiplier (Connected → 1.0, Disconnected → 0.55).
    brightness: f64,
    /// Eased rotation speed (rad/s).
    rot_speed: f64,
}

impl PlanetState {
    fn new() -> Self {
        Self {
            t: 0.0,
            yaw: 0.0,
            brightness: 0.55,
            rot_speed: 0.18,
        }
    }

    fn tick(&mut self, dt: f64, phase: ConnPhase) {
        self.t += dt;

        let connected = matches!(phase, ConnPhase::Connected | ConnPhase::Disconnecting);

        let tgt_brightness = if connected { 1.0 } else { 0.55 };
        let tgt_speed      = if connected { 0.32 } else { 0.18 };

        let ease = |x: f64, tgt: f64, r: f64| x + (tgt - x) * (r * dt).min(1.0);

        self.brightness = ease(self.brightness, tgt_brightness, 2.5);
        self.rot_speed  = ease(self.rot_speed, tgt_speed, 1.5);
        self.yaw       += self.rot_speed * dt;
    }
}

// ── PlanetSkin ────────────────────────────────────────────────────────────────

/// Fibonacci-sphere dotted globe skin.
pub struct PlanetSkin {
    state: PlanetState,
    sphere: [SpherePoint; N_POINTS],
}

impl PlanetSkin {
    pub fn new() -> Self {
        Self {
            state: PlanetState::new(),
            sphere: build_fibonacci_sphere(),
        }
    }
}

impl Default for PlanetSkin {
    fn default() -> Self {
        Self::new()
    }
}

impl Skin for PlanetSkin {
    fn render(&mut self, ctx: &mut dyn RenderContext, st: CenterpieceState) {
        self.state.tick(st.dt, st.phase);
        render_planet(ctx, &self.state, &self.sphere, st);
    }
}

// ── Render ────────────────────────────────────────────────────────────────────

fn render_planet(
    ctx: &mut dyn RenderContext,
    state: &PlanetState,
    sphere: &[SpherePoint; N_POINTS],
    frame: CenterpieceState,
) {
    let rect = frame.rect;

    // Background.
    ctx.set_fill_color("#060810");
    ctx.fill_rect(rect.x, rect.y, rect.w, rect.h);

    // Dot grid (shared look).
    draw_dot_grid(ctx, &rect);

    let cx = rect.cx();
    let cy = rect.cy();
    let radius = rect.w.min(rect.h) * 0.30;

    // Subtle equator ring.
    ctx.set_stroke_color("rgba(80,140,200,0.10)");
    ctx.set_stroke_width(0.8);
    ctx.begin_path();
    ctx.arc(cx, cy, radius, 0.0, TAU);
    ctx.stroke();

    let yaw_cos = state.yaw.cos();
    let yaw_sin = state.yaw.sin();

    // Draw back-hemisphere dots first (z_rot < 0), then front on top.
    let mut points: Vec<(f64, f64, f64)> = sphere
        .iter()
        .map(|p| {
            // Rotate around Y axis by `yaw`.
            let rx = p.x * yaw_cos + p.z * yaw_sin;
            let ry = p.y;
            let rz = -p.x * yaw_sin + p.z * yaw_cos;
            // Orthographic projection: screen_x = cx + rx*radius,
            //                          screen_y = cy - ry*radius (Y up).
            let sx = cx + rx * radius;
            let sy = cy - ry * radius;
            (sx, sy, rz) // rz as depth
        })
        .collect();

    // Sort back-to-front so front dots paint on top.
    points.sort_unstable_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

    for (sx, sy, rz) in &points {
        // z in [-1, 1]; front hemisphere z > 0.
        let z_norm = (*rz + 1.0) * 0.5; // [0, 1], 1 = front
        let alpha = (0.08 + 0.82 * z_norm * z_norm) * state.brightness;
        if alpha < 0.02 {
            continue;
        }

        // Dot size: front dots slightly larger.
        let dot_r = 0.8 + 0.8 * z_norm;

        // Color: front = cyan-white, back = deep blue.
        let r = (120.0 + 130.0 * z_norm) as u8;
        let g = (160.0 + 80.0  * z_norm) as u8;
        let b = (220.0 + 30.0  * z_norm) as u8;
        let color = format!("rgba({r},{g},{b},{:.3})", alpha.clamp(0.0, 1.0));

        ctx.set_fill_color(&color);
        ctx.begin_path();
        ctx.arc(*sx, *sy, dot_r, 0.0, TAU);
        ctx.fill();
    }

    // Soft radial glow when connected.
    if state.brightness > 0.6 {
        let glow_alpha = ((state.brightness - 0.6) / 0.4) * 0.18;
        let color = format!("rgba(80,180,255,{:.3})", glow_alpha);
        ctx.set_fill_color(&color);
        ctx.begin_path();
        ctx.arc(cx, cy, radius * 1.15, 0.0, TAU);
        ctx.fill();
    }
}

fn draw_dot_grid(ctx: &mut dyn RenderContext, rect: &SkinRect) {
    const STEP: f64 = 24.0;
    ctx.set_fill_color("rgba(255,255,255,0.025)");
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
