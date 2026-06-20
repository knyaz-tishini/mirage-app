//! TapDetector — single-tap with 280 ms confirmation window.

use std::time::Instant;

/// A confirmed single-tap event.
pub enum TapEvent {
    Single,
}

/// Detects a single-tap gesture from pointer-down events.
///
/// A tap is confirmed 280 ms after `on_pointer_down` unless the pointer
/// moves > 8 px (drag threshold — shell responsibility to check before
/// forwarding pointer-down here).
pub struct TapDetector {
    pending_since: Option<Instant>,
    last_down: Option<(f64, f64)>,
    fired: bool,
}

impl TapDetector {
    pub fn new() -> Self {
        Self {
            pending_since: None,
            last_down: None,
            fired: false,
        }
    }

    /// Call when the pointer is pressed at `(x, y)`.
    pub fn on_pointer_down(&mut self, x: f64, y: f64) {
        self.pending_since = Some(Instant::now());
        self.last_down = Some((x, y));
        self.fired = false;
    }

    /// Call when the pointer is released.  Returns `Some(TapEvent::Single)`
    /// immediately if the press-to-release was short (< 280 ms).
    pub fn on_pointer_up(&mut self, x: f64, y: f64) -> Option<TapEvent> {
        let (px, py) = self.last_down?;
        let travel = ((x - px).powi(2) + (y - py).powi(2)).sqrt();
        if travel > 8.0 {
            // Drag — not a tap.
            self.pending_since = None;
            return None;
        }
        if let Some(since) = self.pending_since {
            if since.elapsed().as_millis() < 280 && !self.fired {
                self.fired = true;
                self.pending_since = None;
                return Some(TapEvent::Single);
            }
        }
        None
    }

}

impl Default for TapDetector {
    fn default() -> Self {
        Self::new()
    }
}
