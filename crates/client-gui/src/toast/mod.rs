//! ToastState — one-liner status messages with auto-fade.

use std::time::Instant;

/// A transient status message shown at the bottom of the scene.
pub struct ToastState {
    message: Option<String>,
    shown_at: Option<Instant>,
    /// Duration before the message starts fading out (seconds).
    display_secs: f64,
    /// Duration of the fade-out (seconds).
    fade_secs: f64,
}

impl ToastState {
    pub fn new() -> Self {
        Self {
            message: None,
            shown_at: None,
            display_secs: 2.5,
            fade_secs: 0.6,
        }
    }

    /// Show a toast message.
    pub fn show(&mut self, msg: impl Into<String>) {
        self.message = Some(msg.into());
        self.shown_at = Some(Instant::now());
    }

    /// Current alpha (1.0 = fully visible, 0.0 = invisible).
    pub fn alpha(&self) -> f64 {
        let Some(shown_at) = self.shown_at else {
            return 0.0;
        };
        let elapsed = shown_at.elapsed().as_secs_f64();
        if elapsed < self.display_secs {
            1.0
        } else {
            let fade_progress = (elapsed - self.display_secs) / self.fade_secs;
            (1.0 - fade_progress).clamp(0.0, 1.0)
        }
    }

    /// Current message text if visible.
    pub fn current(&self) -> Option<&str> {
        if self.alpha() > 0.0 {
            self.message.as_deref()
        } else {
            None
        }
    }
}

impl Default for ToastState {
    fn default() -> Self {
        Self::new()
    }
}
