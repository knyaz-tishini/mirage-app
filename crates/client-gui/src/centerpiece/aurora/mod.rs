//! AuroraSkin — a full-screen ASCII cell-shader scene (uzor `ui::effects::text`).
//!
//! An interference / flow field (layered sines) drawn as a character field and
//! coloured iridescent (hsl). The cursor domain-warps it like the radar, but the
//! warp RADIUS scales with cursor SPEED: a fast-moving pointer carves a big wake
//! the field must flow around, a hovering pointer carves a small pocket the field
//! laps tightly around. Velocity is smoothed so the wake trails and settles.

use uzor::render::RenderContext;
use uzor::ui::effects::text::{density_char, hsl, AsciiGrid, Cell, CellShader, Coord, Cursor, GridContext};

use crate::centerpiece::{CenterpieceState, Skin, SkinRect};

/// Logical px per character cell.
const CELL: f64 = 11.0;

// ── Per-cell shader ──────────────────────────────────────────────────────────────

struct AuroraShader;

impl CellShader for AuroraShader {
    fn main(&self, coord: Coord, ctx: &GridContext, cursor: &Cursor) -> Cell {
        let t = ctx.time;
        // Cursor domain-warp in cell space. Not a flat circle: the radius
        // wobbles per-angle (a physical droplet) and the push uses a smooth
        // falloff so the field rounds and flows around it. Radius grows with
        // cursor.intensity (= smoothed speed): fast → big wake, still → tight.
        let mut cx = coord.x as f64;
        let mut cy = coord.y as f64;
        let mut fade = 1.0;
        if cursor.inside {
            let dx = cx - cursor.x;
            let dy = cy - cursor.y;
            let d = (dx * dx + dy * dy).sqrt().max(1e-4);
            let ang = dy.atan2(dx);
            let wob = (ang * 3.0 + t * 3.5).sin() * 0.16 + (ang * 5.0 - t * 2.3).sin() * 0.10;
            let radius = (5.0 + cursor.intensity * 22.0) * (1.0 + wob);
            if d < radius {
                let nd = (d / radius).clamp(0.0, 1.0);
                // smootherstep falloff → rounded drop, strongest at the centre.
                let s = 1.0 - nd * nd * (3.0 - 2.0 * nd);
                let amp = s * radius * 0.62;
                cx += dx / d * amp;
                cy += dy / d * amp;
                fade = (nd / 0.45).clamp(0.0, 1.0); // void hollow at the centre
            }
        }

        // Interference / flow field at the (warped) sample. Lower spatial freq =
        // bigger, smoother features; faster time = livelier.
        let fx = cx * 0.07;
        let fy = cy * 0.07;
        let p = (fx + t * 1.0).sin()
            + (fy - t * 0.85).sin()
            + (fx * 0.6 + fy * 0.9 + t * 0.7).sin()
            + ((fx * fx + fy * fy).sqrt() * 0.6 - t * 1.1).sin();
        let base = (p * 0.125 + 0.5).clamp(0.0, 1.0);
        let v = base.powf(1.6) * fade;

        if v < 0.12 {
            return Cell { ch: ' ', color: [0, 0, 0], alpha: 1.0, scale: 0.0 };
        }

        // Iridescent hue + size-graded glyph (smooth texture transitions).
        let hue = (p * 45.0 + t * 50.0 + coord.y as f64 * 3.0).rem_euclid(360.0);
        let color = hsl(hue, 0.62, 0.32 + 0.42 * v);
        let scale = (0.3 + 1.1 * v).clamp(0.3, 1.35) as f32;
        Cell { ch: density_char(v), color, alpha: 1.0, scale }
    }
}

// ── Skin ──────────────────────────────────────────────────────────────────────────

pub struct AuroraSkin {
    grid: AsciiGrid,
    shader: AuroraShader,
    t: f64,
    last_ptr: Option<(f64, f64)>,
    /// Smoothed cursor speed mapped to 0..1 (drives the warp radius).
    intensity: f64,
}

impl AuroraSkin {
    pub fn new() -> Self {
        Self {
            grid: AsciiGrid::new(1, 1),
            shader: AuroraShader,
            t: 0.0,
            last_ptr: None,
            intensity: 0.25,
        }
    }
}

impl Default for AuroraSkin {
    fn default() -> Self {
        Self::new()
    }
}

impl Skin for AuroraSkin {
    fn render(&mut self, ctx: &mut dyn RenderContext, frame: CenterpieceState) {
        let rect: SkinRect = frame.rect;
        self.t += frame.dt;

        ctx.set_fill_color("#0B0B0E");
        ctx.fill_rect(rect.x, rect.y, rect.w, rect.h);

        let cols = (rect.w / CELL).floor().max(1.0) as usize;
        let rows = (rect.h / CELL).floor().max(1.0) as usize;
        if self.grid.cols() != cols || self.grid.rows() != rows {
            self.grid = AsciiGrid::new(cols, rows);
        }
        let ox = rect.x + (rect.w - cols as f64 * CELL) / 2.0;
        let oy = rect.y + (rect.h - rows as f64 * CELL) / 2.0;

        // Cursor + speed-driven intensity. `frame.pointer` is None when the
        // cursor is outside the window — then the warp fades out and heals.
        let dt = frame.dt.max(1e-3);
        let in_rect = matches!(frame.pointer, Some((px, py))
            if px >= rect.x && px < rect.x + rect.w && py >= rect.y && py < rect.y + rect.h);
        let target = if in_rect {
            let (px, py) = frame.pointer.unwrap();
            let t = if let Some((lx, ly)) = self.last_ptr {
                let speed = ((px - lx).hypot(py - ly)) / dt; // px/s
                (0.22 + speed / 2000.0).clamp(0.22, 1.0) // resting floor → big wake
            } else {
                0.22
            };
            self.last_ptr = Some((px, py));
            t
        } else {
            0.0 // keep last_ptr; the warp shrinks at that spot and heals
        };
        // Rise fast (wake forms), settle slowly (trails, then laps tight).
        let rate = if target > self.intensity { 12.0 } else { 3.5 };
        self.intensity += (target - self.intensity) * (rate * dt).min(1.0);

        // Warp at the live pointer, or the fading last position until settled.
        let warp = frame.pointer.or(self.last_ptr);
        let cursor = match warp {
            Some((px, py)) if self.intensity > 0.02 => Cursor {
                x: (px - ox) / CELL,
                y: (py - oy) / CELL,
                pressed: false,
                inside: true,
                intensity: self.intensity,
            },
            _ => {
                self.last_ptr = None;
                Cursor::default()
            }
        };
        self.grid.set_cursor(cursor);

        self.grid.step(&self.shader, self.t, 1.0);
        self.grid.render(ctx, ox, oy, CELL, CELL);
    }
}
