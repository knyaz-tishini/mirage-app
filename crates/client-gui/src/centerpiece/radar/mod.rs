//! RadarSkin — an ASCII cell-shader scene (`uzor_text::ascii` engine).
//!
//! The radar (moving rings + rotating sweep) is rendered as a character field
//! by a per-cell shader. The cursor domain-warps the field: cells near the
//! pointer are pushed outward so the pattern PARTS and flows around it like
//! water, and the void right under the pointer fades to spaces — the "fill the
//! empty space under the cursor" behaviour the pretext-canvas idea is after.

use std::f64::consts::TAU;

use uzor::render::RenderContext;
use uzor_text::ascii::{density_char, AsciiGrid, Cell, CellShader, Coord, Cursor, GridContext};

use crate::centerpiece::{CenterpieceState, Skin, SkinRect};

/// Logical px per character cell.
const CELL: f64 = 10.0;

// ── The per-cell radar shader ────────────────────────────────────────────────────

struct RadarShader;

#[inline]
fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * t.clamp(0.0, 1.0)) as u8
}

impl CellShader for RadarShader {
    fn main(&self, coord: Coord, ctx: &GridContext, cursor: &Cursor) -> Cell {
        let half = (ctx.cols.min(ctx.rows) as f64) / 2.0;
        // Aspect-corrected, centre-origin coords (~[-1, 1]).
        let nx = (coord.x as f64 - ctx.cols as f64 / 2.0 + 0.5) / half * ctx.aspect;
        let ny = (coord.y as f64 - ctx.rows as f64 / 2.0 + 0.5) / half;

        // Cursor domain-warp: push the sampled position away from the cursor so
        // the field parts and flows around it; fade the cells right under it.
        let mut sx = nx;
        let mut sy = ny;
        let mut fade = 1.0;
        if cursor.inside {
            let cnx = (cursor.x - ctx.cols as f64 / 2.0) / half * ctx.aspect;
            let cny = (cursor.y - ctx.rows as f64 / 2.0) / half;
            let dx = nx - cnx;
            let dy = ny - cny;
            let d = (dx * dx + dy * dy).sqrt();
            let radius = 0.55;
            if d < radius {
                let push = (radius - d) / radius; // 1 at cursor → 0 at edge
                let amp = push * push * 0.6; // displacement strength
                if d > 1e-4 {
                    sx = nx + dx / d * amp;
                    sy = ny + dy / d * amp;
                }
                // Void under the pointer: fade to space within the inner region.
                fade = (d / (radius * 0.4)).clamp(0.0, 1.0);
            }
        }

        // Radar field at the (possibly warped) sample.
        let r = (sx * sx + sy * sy).sqrt();
        let ang = sy.atan2(sx);

        // Moving concentric rings.
        let rings = (r * 6.0 - ctx.time * 0.9).sin();
        let ring_b = (rings * 0.5 + 0.5).powf(3.0);

        // Rotating sweep beam — bright at the beam, trailing fade behind it.
        let beam = (ctx.time * 1.1) % TAU;
        let da = (ang - beam).rem_euclid(TAU);
        let sweep = (1.0 - da / TAU).powf(2.5);

        // Fade out beyond the unit circle.
        let edge = (1.0 - r).clamp(0.0, 1.0);
        let v = (ring_b * 0.45 + sweep * 0.8) * edge * fade;

        if v < 0.04 {
            return Cell { ch: ' ', color: [0, 0, 0], alpha: 1.0, scale: 0.0 };
        }

        // Dim amber → bright amber by brightness.
        let dim = [120u8, 86, 50];
        let hi = [255u8, 217, 176];
        let color = [
            lerp_u8(dim[0], hi[0], v),
            lerp_u8(dim[1], hi[1], v),
            lerp_u8(dim[2], hi[2], v),
        ];
        // Size grows with brightness for a smooth density gradient.
        let scale = (0.4 + 0.7 * v) as f32;
        Cell { ch: density_char(v), color, alpha: 1.0, scale }
    }
}

// ── Skin ──────────────────────────────────────────────────────────────────────────

pub struct RadarSkin {
    grid: AsciiGrid,
    shader: RadarShader,
    t: f64,
}

impl RadarSkin {
    pub fn new() -> Self {
        Self { grid: AsciiGrid::new(1, 1), shader: RadarShader, t: 0.0 }
    }
}

impl Default for RadarSkin {
    fn default() -> Self {
        Self::new()
    }
}

impl Skin for RadarSkin {
    fn render(&mut self, ctx: &mut dyn RenderContext, frame: CenterpieceState) {
        let rect: SkinRect = frame.rect;
        self.t += frame.dt;

        // Background.
        ctx.set_fill_color("#0B0B0E");
        ctx.fill_rect(rect.x, rect.y, rect.w, rect.h);

        // Grid sized to the rect (square cells), centred.
        let cols = (rect.w / CELL).floor().max(1.0) as usize;
        let rows = (rect.h / CELL).floor().max(1.0) as usize;
        if self.grid.cols() != cols || self.grid.rows() != rows {
            self.grid = AsciiGrid::new(cols, rows);
        }
        let ox = rect.x + (rect.w - cols as f64 * CELL) / 2.0;
        let oy = rect.y + (rect.h - rows as f64 * CELL) / 2.0;

        // Cursor in grid-cell coords (aligned to the render offset).
        let cursor = match frame.pointer {
            Some((px, py))
                if px >= rect.x && px < rect.x + rect.w && py >= rect.y && py < rect.y + rect.h =>
            {
                Cursor {
                    x: (px - ox) / CELL,
                    y: (py - oy) / CELL,
                    pressed: false,
                    inside: true,
                    intensity: 1.0,
                }
            }
            _ => Cursor::default(),
        };
        self.grid.set_cursor(cursor);

        self.grid.step(&self.shader, self.t, 1.0);
        self.grid.render(ctx, ox, oy, CELL, CELL);
    }
}
