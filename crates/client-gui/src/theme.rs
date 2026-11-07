//! Sheet theme. Light or dark glass. The pick is `sheet-theme` beside the
//! exe (`light` or `dark`), same place as `lang` and `client-mode`. Absent
//! means dark, which is the night sheet.

use std::sync::atomic::{AtomicU8, Ordering};

use uzor::render::RenderContext;
use uzor::Rect;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SheetTheme {
    Dark = 0,
    Light = 1,
}

const UNSET: u8 = 255;
static CUR: AtomicU8 = AtomicU8::new(UNSET);
const RADIUS: f64 = 12.0;

pub fn current() -> SheetTheme {
    match CUR.load(Ordering::Relaxed) {
        0 => SheetTheme::Dark,
        1 => SheetTheme::Light,
        _ => {
            let theme = stored().unwrap_or(SheetTheme::Dark);
            CUR.store(theme as u8, Ordering::Relaxed);
            theme
        }
    }
}

pub fn light() -> bool {
    current() == SheetTheme::Light
}

pub fn cycle() {
    let next = match current() {
        SheetTheme::Dark => SheetTheme::Light,
        SheetTheme::Light => SheetTheme::Dark,
    };
    CUR.store(next as u8, Ordering::Relaxed);
    let _ = write_stored(next);
}

/// Segmented switch, the same one the chrome uses for mesh / own server.
/// `active` is the chosen cell in track coordinates. The fill sits inset
/// so the track stays visible around it.
pub fn paint_segment(ctx: &mut dyn RenderContext, track: Rect, active: Rect) {
    let light = light();
    ctx.set_fill_color(if light { "rgba(255,255,255,0.34)" } else { "rgba(255,255,255,0.18)" });
    ctx.fill_rounded_rect(track.x, track.y, track.width, track.height, 10.0);
    let cell = Rect::new(
        active.x + 2.0,
        active.y + 2.0,
        (active.width - 4.0).max(0.0),
        (active.height - 4.0).max(0.0),
    );
    ctx.set_fill_color(if light { "rgba(255,255,255,0.86)" } else { "rgba(255,255,255,0.32)" });
    ctx.fill_rounded_rect(cell.x, cell.y, cell.width, cell.height, 8.0);
}

/// Alphabetic baseline that centres the em box in a row. Same line box as
/// the text field (`1.5em`, Inter vertical metrics). Urx `Middle` is a
/// fixed `0.35em` and does not sit on that line, so a label and the field
/// next to it land at different heights.
pub fn label_baseline(top: f64, height: f64, px: f64) -> f64 {
    let lh = (px * 1.5).round();
    let y = top + (height - lh) / 2.0;
    let ascent = 1984.0 / 2048.0;
    let descent = 494.0 / 2048.0;
    y + (lh - (ascent + descent) * px) / 2.0 + ascent * px
}

/// Translucent sheet from the top of the window. The top edge stays square
/// against the caption. The bottom radius matches the pills. The corner is
/// the same antialiased rounded-rect as the buttons. A tessellated arc reads
/// as a jagged, oversized curve.
pub fn paint_sheet(ctx: &mut dyn RenderContext, x: f64, y: f64, w: f64, h: f64) {
    if w < 2.0 || h < 2.0 {
        return;
    }
    let light = light();
    let fill = if light { "rgba(255,255,255,0.62)" } else { "rgba(14,14,17,0.55)" };
    let outer = if light { "rgba(255,255,255,0.78)" } else { "rgba(255,255,255,0.24)" };
    let inner = if light { "rgba(255,255,255,0.32)" } else { "rgba(255,255,255,0.09)" };
    fill_bottom_round(ctx, x, y, w, h, RADIUS, fill);
    stroke_bottom_round(ctx, x + 0.5, y + 0.5, (w - 1.0).max(0.0), (h - 1.0).max(0.0), RADIUS, outer);
    stroke_bottom_round(
        ctx,
        x + 2.0,
        y + 2.0,
        (w - 4.0).max(0.0),
        (h - 4.0).max(0.0),
        (RADIUS - 2.0).max(0.0),
        inner,
    );
}

fn fill_bottom_round(ctx: &mut dyn RenderContext, x: f64, y: f64, w: f64, h: f64, r: f64, color: &str) {
    ctx.set_fill_color(color);
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if r < 1.0 || h <= r * 2.0 {
        ctx.fill_rounded_rect(x, y, w, h, r);
        return;
    }
    // Body, then the bottom cap clipped to the lower band. The two fills
    // share no pixels: a translucent stadium under a body rect stacks, and
    // on the light sheet that stack reads as a brighter ellipse.
    let seam = y + h - r;
    ctx.fill_rect(x, y, w, (h - r).max(0.0));
    ctx.save();
    ctx.clip_rect(x - 1.0, seam, w + 2.0, r + 2.0);
    ctx.fill_rounded_rect(x, seam - r, w, r * 2.0, r);
    ctx.restore();
}

fn stroke_bottom_round(ctx: &mut dyn RenderContext, x: f64, y: f64, w: f64, h: f64, r: f64, color: &str) {
    if w < 2.0 || h < 2.0 {
        return;
    }
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    ctx.set_stroke_color(color);
    ctx.set_stroke_width(1.0);
    ctx.set_line_dash(&[]);
    if r >= 1.0 {
        ctx.save();
        ctx.clip_rect(x - 1.0, y + h - r - 1.5, w + 2.0, r + 3.5);
        ctx.stroke_rounded_rect(x, y, w, h, r);
        ctx.restore();
    }
    let yb = if r >= 1.0 { y + h - r } else { y + h };
    ctx.begin_path();
    ctx.move_to(x, yb);
    ctx.line_to(x, y);
    ctx.line_to(x + w, y);
    ctx.line_to(x + w, yb);
    ctx.stroke();
}

fn stored() -> Option<SheetTheme> {
    let path = std::env::current_exe().ok()?.parent()?.join("sheet-theme");
    let text = std::fs::read_to_string(path).ok()?;
    match text.trim() {
        "light" => Some(SheetTheme::Light),
        "dark" => Some(SheetTheme::Dark),
        _ => None,
    }
}

fn write_stored(theme: SheetTheme) -> std::io::Result<()> {
    let path = std::env::current_exe()?.parent().ok_or(std::io::Error::other("no exe dir"))?.join("sheet-theme");
    let body = match theme {
        SheetTheme::Light => "light\n",
        SheetTheme::Dark => "dark\n",
    };
    std::fs::write(path, body)
}
