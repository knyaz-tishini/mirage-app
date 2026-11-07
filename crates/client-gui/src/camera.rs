//! Camera QR scan — a background capture thread that opens the default camera,
//! decodes every frame with `rqrr`, and reports the first QR payload it finds.
//!
//! The UI can't reach an `ImagePainter` through `&mut dyn RenderContext`, so the
//! live preview is an ASCII luma grid (the capture thread downsamples each frame
//! to `PREV_COLS × rows` so the UI thread only maps luma → density glyph). This
//! also fits the app's ASCII aesthetic.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

#[cfg(not(target_os = "ios"))]
use nokhwa::pixel_format::RgbFormat;
#[cfg(not(target_os = "ios"))]
use nokhwa::utils::{CameraIndex, RequestedFormat, RequestedFormatType};
#[cfg(not(target_os = "ios"))]
use nokhwa::Camera;
use uzor::render::{RenderContext, TextAlign, TextBaseline};

/// Preview grid width in glyph columns; rows derive from the frame aspect.
pub const PREV_COLS: usize = 80;

/// Outcome of the scan, polled by the UI each frame.
#[derive(Clone)]
pub enum ScanOutcome {
    /// Still scanning — no QR yet.
    Scanning,
    /// A QR payload was decoded.
    Done(String),
    /// The camera could not be opened / read.
    Failed(String),
}

/// Latest ASCII preview frame: `cols × rows` luma bytes (0=dark, 255=light).
#[derive(Default)]
pub struct Preview {
    pub cols: usize,
    pub rows: usize,
    pub luma: Vec<u8>,
}

struct Shared {
    outcome: Mutex<ScanOutcome>,
    preview: Mutex<Preview>,
}

/// A running camera scan. Drop (or `cancel`) stops the capture thread.
pub struct CameraScan {
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
    /// When the scan started — drives the caption animation / timeout.
    pub started: Instant,
}

impl CameraScan {
    /// Open the default camera and begin scanning in a background thread.
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            outcome: Mutex::new(ScanOutcome::Scanning),
            preview: Mutex::new(Preview::default()),
        });
        #[cfg(not(target_os = "ios"))]
        let stop_t = stop.clone();
        #[cfg(not(target_os = "ios"))]
        let shared_t = shared.clone();
        #[cfg(target_os = "ios")]
        {
            *shared.outcome.lock().unwrap() = ScanOutcome::Failed(crate::text::tr("cam.down").into());
            return Self { stop, shared, join: None, started: Instant::now() };
        }
        #[cfg(not(target_os = "ios"))]
        let join = std::thread::Builder::new()
            .name("mirage-cam-scan".into())
            .spawn(move || capture_loop(&stop_t, &shared_t))
            .ok();
        #[cfg(not(target_os = "ios"))]
        Self { stop, shared, join, started: Instant::now() }
    }

    /// Current scan outcome (cheap clone).
    pub fn outcome(&self) -> ScanOutcome {
        self.shared.outcome.lock().unwrap().clone()
    }

    /// Run a closure over the latest preview frame without copying it.
    pub fn with_preview<R>(&self, f: impl FnOnce(&Preview) -> R) -> R {
        f(&self.shared.preview.lock().unwrap())
    }

    /// Signal the capture thread to stop.
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for CameraScan {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// The background capture + decode loop. Writes results into `shared`.
#[cfg(not(target_os = "ios"))]
fn capture_loop(stop: &AtomicBool, shared: &Shared) {
    let index = CameraIndex::Index(0);
    let fmt = RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution);
    let mut cam = match Camera::new(index, fmt) {
        Ok(c) => c,
        Err(e) => {
            *shared.outcome.lock().unwrap() = ScanOutcome::Failed(format!("{}: {e}", crate::text::tr("cam.down")));
            return;
        }
    };
    if let Err(e) = cam.open_stream() {
        *shared.outcome.lock().unwrap() = ScanOutcome::Failed(format!("{}: {e}", crate::text::tr("cam.start")));
        return;
    }

    while !stop.load(Ordering::Relaxed) {
        let frame = match cam.frame() {
            Ok(f) => f,
            Err(_) => continue, // transient read miss — retry
        };
        let img = match frame.decode_image::<RgbFormat>() {
            Ok(i) => i, // image::RgbImage (RGB, 3 bytes/px) — type left to inference
            Err(_) => continue,
        };
        let (w, h) = (img.width() as usize, img.height() as usize);
        let rgb: &[u8] = &img; // ImageBuffer derefs to the raw byte slice
        if w == 0 || h == 0 || rgb.len() < w * h * 3 {
            continue;
        }

        // Update the ASCII preview (box-downsample to PREV_COLS × rows).
        update_preview(shared, rgb, w, h);

        // Decode QR straight off a grayscale closure (no image codec dep).
        let mut prep = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| {
            let i = (y * w + x) * 3;
            let (r, g, b) = (rgb[i] as u32, rgb[i + 1] as u32, rgb[i + 2] as u32);
            ((r * 299 + g * 587 + b * 114) / 1000) as u8
        });
        for grid in prep.detect_grids() {
            if let Ok((_m, content)) = grid.decode() {
                *shared.outcome.lock().unwrap() = ScanOutcome::Done(content);
                return;
            }
        }
    }
}

/// ASCII density ramp, dark → light.
const RAMP: &[u8] = b" .:-=+*#%@";

/// Draw the fullscreen scan overlay (dim + ASCII camera preview + caption) into
/// the area below the chrome strip. Only ever called while the scan is running.
pub fn draw_overlay(ctx: &mut dyn RenderContext, w: f64, h: f64, chrome_h: f64, scan: &CameraScan) {
    // Dim the scene behind the preview.
    ctx.set_fill_color("rgba(8,8,11,0.94)");
    ctx.fill_rect(0.0, chrome_h, w, (h - chrome_h).max(0.0));

    let top = chrome_h + 22.0;
    let bottom = h - 60.0;
    let (cols, rows, has_frame) = scan.with_preview(|p| (p.cols, p.rows, !p.luma.is_empty()));

    if has_frame && cols > 0 && rows > 0 && bottom > top {
        // Fill the available area; glyphs are ~2× taller than wide so a square
        // pixel maps to cellw ≈ cellh/2 — the natural monospace pitch.
        let ox = 12.0;
        let oy = top;
        let cellw = (w - 24.0).max(1.0) / cols as f64;
        let cellh = (bottom - top).max(1.0) / rows as f64;

        ctx.set_font(&format!("{}px monospace", (cellh * 1.05).round().max(6.0) as i32));
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);

        let mut last_bucket = usize::MAX;
        let mut buf = [0u8; 4];
        scan.with_preview(|p| {
            for ry in 0..p.rows {
                for cx in 0..p.cols {
                    let luma = p.luma[ry * p.cols + cx] as usize;
                    let bucket = luma / 32; // 8 brightness levels → fewer color swaps
                    if bucket != last_bucket {
                        let a = 0.20 + 0.80 * (bucket as f64 / 7.0);
                        ctx.set_fill_color(&format!("rgba(251,178,106,{a:.2})"));
                        last_bucket = bucket;
                    }
                    let ch = RAMP[luma * (RAMP.len() - 1) / 255] as char;
                    let s = ch.encode_utf8(&mut buf);
                    ctx.fill_text(s, ox + (cx as f64 + 0.5) * cellw, oy + (ry as f64 + 0.5) * cellh);
                }
            }
        });
    } else {
        // No frame yet — camera is spinning up.
        ctx.set_fill_color("#878B91");
        ctx.set_font("13px monospace");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text(crate::text::tr("cam.opening"), w / 2.0, (top + bottom) / 2.0);
    }

    // Caption with an animated ellipsis driven by elapsed time.
    let dots = ((scan.started.elapsed().as_millis() / 400) % 4) as usize;
    let caption = format!(
        "{}{}  ·  {}",
        crate::text::tr("cam.scan"),
        ".".repeat(dots),
        crate::text::tr("cam.cancel")
    );
    ctx.set_fill_color("#FBB26A");
    ctx.set_font("12px sans-serif");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Bottom);
    ctx.fill_text(&caption, w / 2.0, h - 22.0);
}

/// Box-downsample an RGB frame to a `PREV_COLS × rows` luma grid for ASCII
/// preview, then min-max contrast-stretch it so even a dim frame stays legible.
fn update_preview(shared: &Shared, rgb: &[u8], w: usize, h: usize) {
    let cols = PREV_COLS.min(w);
    let rows = ((cols * h) / w).max(1);
    let mut luma = vec![0u8; cols * rows];
    let (mut lo, mut hi) = (255u8, 0u8);
    for ry in 0..rows {
        for cx in 0..cols {
            // Sample the source pixel at the cell centre (cheap, good enough).
            let sx = (cx * w) / cols + w / (cols * 2);
            let sy = (ry * h) / rows + h / (rows * 2);
            let i = (sy.min(h - 1) * w + sx.min(w - 1)) * 3;
            let (r, g, b) = (rgb[i] as u32, rgb[i + 1] as u32, rgb[i + 2] as u32);
            let l = ((r * 299 + g * 587 + b * 114) / 1000) as u8;
            luma[ry * cols + cx] = l;
            lo = lo.min(l);
            hi = hi.max(l);
        }
    }
    // Contrast stretch + mild gamma lift (skip if the frame is flat) so a dim
    // scene still reveals midtone structure instead of collapsing to spaces.
    if hi > lo {
        let span = (hi - lo) as f64;
        for l in luma.iter_mut() {
            let n = (*l - lo) as f64 / span; // 0..1
            *l = (n.powf(0.65) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    *shared.preview.lock().unwrap() = Preview { cols, rows, luma };
}
