//! PlanetSkin — morph scene: grid relief → fold into a real-continents globe →
//! mesh of fleet nodes. Faithful port of `morph-scene.html`.
//!
//! Disconnected: flat grid relief (height-noise plane). Connecting: the grid
//! folds into a sphere. Connected: dotted real continents (baked Natural-Earth
//! land mask) + the 6 fleet nodes geo-pinned with great-circle mesh links.
//! Auto-rotates around the Y axis throughout.

mod landmask;

use std::f64::consts::{PI, TAU};

use uzor::render::RenderContext;

use crate::centerpiece::{CenterpieceState, ConnPhase, Skin, SkinRect};

const DEG: f64 = 180.0 / PI;

// ── Grid ────────────────────────────────────────────────────────────────────────
const NG: usize = 50;
const N: usize = NG * NG;
const NOISE_SEED: u32 = 4242;

// White → amber endpoints (lerp by warmth).
const COOL: [f64; 3] = [244.0, 244.0, 245.0];
const WARM: [f64; 3] = [251.0, 178.0, 106.0];

// ── Hash + value noise (ports of ihash / vnoise) ─────────────────────────────────

fn ihash(i: i32, j: i32, s: u32) -> f64 {
    let h = (i as i64 * 374_761_393 + j as i64 * 668_265_263 + s as i64) as i32;
    let h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    ((h ^ (h >> 16)) as u32) as f64 / 4_294_967_296.0
}

fn vnoise(x: f64, y: f64, s: u32) -> f64 {
    let xi = x.floor();
    let yi = y.floor();
    let xf = x - xi;
    let yf = y - yi;
    let u = xf * xf * (3.0 - 2.0 * xf);
    let v = yf * yf * (3.0 - 2.0 * yf);
    let (xi, yi) = (xi as i32, yi as i32);
    let a = ihash(xi, yi, s);
    let b = ihash(xi + 1, yi, s);
    let c = ihash(xi, yi + 1, s);
    let d = ihash(xi + 1, yi + 1, s);
    a + (b - a) * u + (c - a) * v + (a - b - c + d) * u * v
}

#[inline]
fn clamp(v: f64, a: f64, b: f64) -> f64 {
    v.max(a).min(b)
}
#[inline]
fn smooth(u: f64) -> f64 {
    u * u * (3.0 - 2.0 * u)
}
#[inline]
fn ez(x: f64, tgt: f64, rate: f64, dt: f64) -> f64 {
    x + (tgt - x) * (rate * dt).min(1.0)
}

// ── Grid point ───────────────────────────────────────────────────────────────────
#[derive(Clone, Copy)]
struct GridPt {
    px: f64,
    pz: f64,
    fib: [f64; 3],
    i: i32,
    j: i32,
    land: bool,
}

#[derive(Clone, Copy)]
struct Node {
    p: [f64; 3],
    /// Neutral, customer-safe label — see `crate::nodes::FleetNode::label`.
    label: &'static str,
}

fn geo(lat: f64, lon: f64) -> [f64; 3] {
    let la = lat / DEG;
    let lo = lon / DEG;
    [la.cos() * lo.cos(), la.sin(), -la.cos() * lo.sin()]
}

// ── Skin ──────────────────────────────────────────────────────────────────────────
pub struct PlanetSkin {
    pts: Vec<GridPt>,
    nodes: Vec<Node>,
    links: Vec<(usize, usize)>,
    t: f64,
    yaw: f64,
    pitch: f64,
    vyaw: f64,
    vpitch: f64,
    zoom: f64,
    /// Frames since the last drag input (suppresses auto-rotate while dragging).
    since_drag: u32,
    fold: f64,
    amp: f64,
    col_t: f64,
    mesh: f64,
    /// Last-frame projected node screen positions (x, y, front?).
    node_screen: Vec<(f64, f64, bool)>,
    /// Currently selected node (info popup), if any.
    sel: Option<usize>,
}

impl PlanetSkin {
    pub fn new() -> Self {
        let ga = PI * (3.0 - 5.0_f64.sqrt());
        let mut pts = Vec::with_capacity(N);
        for i in 0..NG {
            for j in 0..NG {
                let u = i as f64 / (NG as f64 - 1.0);
                let v = j as f64 / (NG as f64 - 1.0);
                let k = i * NG + j;
                let fy = 1.0 - (k as f64 / (N as f64 - 1.0)) * 2.0;
                let fr = (1.0 - fy * fy).max(0.0).sqrt();
                let fth = ga * k as f64;
                let fib = [fth.cos() * fr, fy, fth.sin() * fr];
                let latd = clamp(fy, -1.0, 1.0).asin() * DEG;
                let lond = -fib[2].atan2(fib[0]) * DEG;
                pts.push(GridPt {
                    px: (u - 0.5) * 2.0,
                    pz: (v - 0.5) * 2.0,
                    fib,
                    i: i as i32,
                    j: j as i32,
                    land: landmask::is_land(latd, lond),
                });
            }
        }

        // Shared fleet inventory (single source — see crate::nodes).
        let nodes = crate::nodes::FLEET
            .iter()
            .map(|f| Node {
                p: geo(f.lat, f.lon),
                label: f.label,
            })
            .collect();
        let links = crate::nodes::LINKS.to_vec();

        Self {
            pts,
            nodes,
            links,
            t: 0.0,
            yaw: 0.5,
            pitch: 0.38,
            vyaw: 0.0,
            vpitch: 0.0,
            zoom: 1.0,
            since_drag: 999,
            fold: 0.0,
            amp: 0.16,
            col_t: 0.0,
            mesh: 0.0,
            node_screen: vec![(0.0, 0.0, false); 6],
            sel: None,
        }
    }

    fn rot(&self, p: [f64; 3]) -> [f64; 3] {
        let (cyw, syw) = (self.yaw.cos(), self.yaw.sin());
        let x1 = p[0] * cyw + p[2] * syw;
        let z1 = -p[0] * syw + p[2] * cyw;
        let y1 = p[1];
        let (cp, sp) = (self.pitch.cos(), self.pitch.sin());
        let y2 = y1 * cp - z1 * sp;
        let z2 = y1 * sp + z1 * cp;
        [x1, y2, z2]
    }

    fn hgt(&self, i: i32, j: i32) -> f64 {
        (vnoise(i as f64 * 0.22, j as f64 * 0.22 + self.t * 0.4, NOISE_SEED) - 0.5)
            + 0.25 * ((i + j) as f64 * 0.42 + self.t * 1.8).sin()
    }
}

impl Default for PlanetSkin {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn lerp3(a: [f64; 3], b: [f64; 3], t: f64) -> [u8; 3] {
    [
        (a[0] + (b[0] - a[0]) * t) as u8,
        (a[1] + (b[1] - a[1]) * t) as u8,
        (a[2] + (b[2] - a[2]) * t) as u8,
    ]
}

impl Skin for PlanetSkin {
    fn render(&mut self, ctx: &mut dyn RenderContext, frame: CenterpieceState) {
        let dt = frame.dt;
        let rect: SkinRect = frame.rect;
        self.t += dt;

        // ── Targets by phase ───────────────────────────────────────────────────
        let folding = matches!(frame.phase, ConnPhase::Connecting | ConnPhase::Connected);
        let warm = folding;
        let meshed = matches!(frame.phase, ConnPhase::Connected);
        let tf = if folding { 1.0 } else { 0.0 };
        let ta = if matches!(frame.phase, ConnPhase::Disconnected) { 0.16 } else { 0.26 };
        let tc = if warm { 1.0 } else { 0.0 };
        let tm = if meshed { 1.0 } else { 0.0 };

        // Ease (morph per-frame rates × 60 → per-second).
        self.fold = ez(self.fold, tf, 0.66, dt);
        self.amp = ez(self.amp, ta, 2.4, dt);
        self.col_t = ez(self.col_t, tc, 3.0, dt);
        self.mesh = ez(self.mesh, tm, 3.6, dt);

        // Rotation: drag() moves yaw/pitch directly (since_drag = 0 that frame);
        // otherwise coast on inertia + a gentle idle auto-rotate.
        if self.since_drag == 0 {
            // Dragged this frame — drag() already moved yaw/pitch.
            self.since_drag = 1;
        } else if self.sel.is_none() {
            // Idle: coast on inertia + a gentle auto-rotate.
            self.yaw += self.vyaw + 0.12 * dt;
            self.pitch = clamp(self.pitch + self.vpitch, -1.35, 1.35);
            self.vyaw *= 0.92;
            self.vpitch *= 0.92;
        } else {
            // Node popup open — hold the globe still so it can be read.
            self.vyaw = 0.0;
            self.vpitch = 0.0;
        }

        let fe = smooth(clamp(self.fold, 0.0, 1.0));
        let mesh_a = self.mesh * clamp((self.fold - 0.9) / 0.08, 0.0, 1.0);
        let line_fade = clamp(1.0 - fe / 0.12, 0.0, 1.0);
        let dot_fade = clamp((fe - 0.03) / 0.16, 0.0, 1.0);

        let cx = rect.cx();
        let cy = rect.y + rect.h * 0.5;
        let r_globe = rect.w.min(rect.h) * 0.40 * self.zoom;

        // ── Background ─────────────────────────────────────────────────────────
        ctx.set_fill_color("#0B0B0E");
        ctx.fill_rect(rect.x, rect.y, rect.w, rect.h);

        // ── Project all grid points ──────────────────────────────────────────────
        let mut pr = vec![[0.0_f64; 3]; N];
        for (k, p) in self.pts.iter().enumerate() {
            let flat_y = self.amp * 1.5 * self.hgt(p.i, p.j);
            let wx = p.px + (p.fib[0] - p.px) * fe;
            let wy = flat_y + (p.fib[1] - flat_y) * fe;
            let wz = p.pz + (p.fib[2] - p.pz) * fe;
            let r = self.rot([wx, wy, wz]);
            pr[k] = [cx + r[0] * r_globe, cy - r[1] * r_globe, r[2]];
        }

        // ── Sphere outline (as it nears a full globe) ────────────────────────────
        if fe > 0.55 {
            let a = 0.06 * clamp((fe - 0.55) / 0.3, 0.0, 1.0);
            ctx.set_stroke_color(&format!("rgba(255,255,255,{:.3})", a));
            ctx.set_stroke_width(1.0);
            ctx.set_line_dash(&[]);
            ctx.begin_path();
            ctx.arc(cx, cy, r_globe, 0.0, TAU);
            ctx.stroke();
        }

        // ── Grid lines (fade out as the relief folds away) ───────────────────────
        if line_fade > 0.01 {
            let c = lerp3(COOL, WARM, self.col_t);
            ctx.set_stroke_color(&format!("rgba({},{},{},{:.3})", c[0], c[1], c[2], 0.45 * line_fade));
            ctx.set_stroke_width(1.0);
            ctx.set_line_dash(&[]);
            for i in 0..NG {
                ctx.begin_path();
                for j in 0..NG {
                    let s = pr[i * NG + j];
                    if j == 0 {
                        ctx.move_to(s[0], s[1]);
                    } else {
                        ctx.line_to(s[0], s[1]);
                    }
                }
                ctx.stroke();
            }
            for j in 0..NG {
                ctx.begin_path();
                for i in 0..NG {
                    let s = pr[i * NG + j];
                    if i == 0 {
                        ctx.move_to(s[0], s[1]);
                    } else {
                        ctx.line_to(s[0], s[1]);
                    }
                }
                ctx.stroke();
            }
        }

        // ── Land dots (real continents) ──────────────────────────────────────────
        if dot_fade > 0.01 {
            let c = lerp3(COOL, WARM, self.col_t);
            for (k, p) in self.pts.iter().enumerate() {
                if !p.land {
                    continue;
                }
                let s = pr[k];
                let d2 = s[2];
                if d2 < -0.04 {
                    continue;
                }
                let s_b = 0.32 + 0.68 * clamp(d2, 0.0, 1.0);
                let tw = 0.85 + 0.15 * (self.t * 2.4 + k as f64 * 0.7).sin();
                let al = s_b * tw * dot_fade;
                ctx.set_fill_color(&format!("rgba({},{},{},{:.3})", c[0], c[1], c[2], al));
                ctx.fill_rect(s[0] - 1.0, s[1] - 1.0, 2.2, 2.2);
            }
        }

        // ── Mesh (fleet nodes + great-circle links) ──────────────────────────────
        if mesh_a > 0.01 {
            // Links.
            ctx.set_stroke_width(1.4);
            ctx.set_line_dash(&[]);
            for &(a, b) in &self.links {
                let ra = self.rot(self.nodes[a].p);
                let rb = self.rot(self.nodes[b].p);
                let mut path: Vec<[f64; 3]> = Vec::with_capacity(21);
                for k in 0..=20 {
                    let tt = k as f64 / 20.0;
                    let mut mx = ra[0] + (rb[0] - ra[0]) * tt;
                    let mut my = ra[1] + (rb[1] - ra[1]) * tt;
                    let mut mz = ra[2] + (rb[2] - ra[2]) * tt;
                    let l = (mx * mx + my * my + mz * mz).sqrt().max(1e-6);
                    mx /= l;
                    my /= l;
                    mz /= l;
                    let lift = 1.0 + 0.16 * (tt * PI).sin();
                    path.push([cx + mx * lift * r_globe, cy - my * lift * r_globe, mz]);
                }
                ctx.set_stroke_color(&format!("rgba(255,217,176,{:.3})", 0.8 * mesh_a));
                ctx.begin_path();
                let mut started = false;
                for pnt in &path {
                    if pnt[2] >= -0.02 {
                        if started {
                            ctx.line_to(pnt[0], pnt[1]);
                        } else {
                            ctx.move_to(pnt[0], pnt[1]);
                            started = true;
                        }
                    } else {
                        started = false;
                    }
                }
                ctx.stroke();
            }

            // Nodes — project, record screen positions, draw.
            let mut screens = vec![(0.0_f64, 0.0_f64, false); self.nodes.len()];
            ctx.set_global_alpha(mesh_a);
            for (ni, n) in self.nodes.iter().enumerate() {
                let r = self.rot(n.p);
                let s = (cx + r[0] * r_globe, cy - r[1] * r_globe);
                let front = r[2] >= -0.02;
                screens[ni] = (s.0, s.1, front);
                if !front {
                    continue;
                }
                if self.sel == Some(ni) {
                    ctx.set_stroke_color(&format!("rgba(251,178,106,{:.3})", 0.8 * mesh_a));
                    ctx.set_stroke_width(1.3);
                    ctx.set_line_dash(&[]);
                    ctx.begin_path();
                    ctx.arc(s.0, s.1, 9.0, 0.0, TAU);
                    ctx.stroke();
                }
                ctx.set_fill_color("#F4F4F5");
                ctx.begin_path();
                ctx.arc(s.0, s.1, 4.6, 0.0, TAU);
                ctx.fill();
                ctx.set_fill_color("#FBB26A");
                ctx.begin_path();
                ctx.arc(s.0, s.1, 2.8, 0.0, TAU);
                ctx.fill();
                ctx.set_fill_color("#FBB26A");
                ctx.set_font("9px monospace");
                ctx.fill_text(n.label, s.0 + 9.0, s.1 - 3.0);
            }
            ctx.set_global_alpha(1.0);
            self.node_screen = screens;

            // Node info popup.
            if let Some(si) = self.sel {
                if let Some(&(sx, sy, front)) = self.node_screen.get(si) {
                    if front && mesh_a > 0.5 {
                        self.draw_popup(ctx, &self.nodes[si], sx, sy, rect);
                    }
                }
            }
        } else {
            self.sel = None;
        }
    }

    fn drag(&mut self, dx: f64, dy: f64) {
        self.yaw += dx * 0.008;
        self.pitch = clamp(self.pitch + dy * 0.008, -1.35, 1.35);
        self.vyaw = dx * 0.008;
        self.vpitch = dy * 0.008;
        self.since_drag = 0;
    }

    fn zoom(&mut self, dy: f64) {
        let f = if dy > 0.0 { 1.08 } else { 0.926 };
        self.zoom = clamp(self.zoom * f, 0.6, 3.2);
    }

    fn tap(&mut self, x: f64, y: f64) -> bool {
        let mut best: Option<usize> = None;
        let mut best_d = 16.0 * 16.0;
        for (i, &(sx, sy, front)) in self.node_screen.iter().enumerate() {
            if !front {
                continue;
            }
            let d = (sx - x) * (sx - x) + (sy - y) * (sy - y);
            if d < best_d {
                best_d = d;
                best = Some(i);
            }
        }
        match best {
            Some(i) => {
                self.sel = if self.sel == Some(i) { None } else { Some(i) };
                true
            }
            // Missed all nodes: if a popup is open, a tap-outside closes it
            // (consumed). Otherwise let the shell treat it as a connect tap.
            None => {
                if self.sel.is_some() {
                    self.sel = None;
                    true
                } else {
                    false
                }
            }
        }
    }

    fn clear_selection(&mut self) {
        self.sel = None;
    }
}

impl PlanetSkin {
    /// Draw the floating node-info popup near `(sx, sy)`, clamped to the
    /// rect. Neutral label only — no IP, node id, provider name, or operator
    /// infra-role string; see `crate::nodes` module doc.
    fn draw_popup(&self, ctx: &mut dyn RenderContext, n: &Node, sx: f64, sy: f64, rect: SkinRect) {
        let header_h = 30.0;
        let ph = header_h + 6.0;

        ctx.set_font("11px monospace");
        let label_w = ctx.measure_text(n.label);
        let label_w = if label_w > 1.0 { label_w } else { n.label.len() as f64 * 6.4 };
        let pw = (label_w + 24.0).max(96.0).min(rect.w - 8.0);

        let px = (sx + 14.0).clamp(rect.x + 4.0, rect.x + rect.w - pw - 4.0);
        let py = (sy - 12.0).clamp(rect.y + 4.0, rect.y + rect.h - ph - 4.0);

        // Panel.
        ctx.set_fill_color("rgba(20,21,26,0.96)");
        ctx.fill_rect(px, py, pw, ph);
        ctx.set_fill_color("#FBB26A");
        ctx.fill_rect(px, py, 2.0, ph);

        // Header: neutral label only.
        ctx.set_fill_color("#FBB26A");
        ctx.set_font("11px monospace");
        ctx.set_text_align(uzor::render::TextAlign::Left);
        ctx.set_text_baseline(uzor::render::TextBaseline::Middle);
        ctx.fill_text(n.label, px + 12.0, py + 16.0);
    }
}
