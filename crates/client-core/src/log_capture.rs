use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tracing::Subscriber;
use tracing_subscriber::Layer;

/// A tracing [`Layer`] that appends formatted log lines into a shared ring buffer.
///
/// When the buffer reaches `max_lines` entries the oldest half is dropped to
/// make room, keeping memory use bounded.
pub struct LogCaptureLayer {
    buffer: Arc<Mutex<VecDeque<String>>>,
    max_lines: usize,
}

impl LogCaptureLayer {
    /// Create a new layer writing into `buffer`, capped at `max_lines` entries.
    pub fn new(buffer: Arc<Mutex<VecDeque<String>>>, max_lines: usize) -> Self {
        Self { buffer, max_lines }
    }
}

impl<S: Subscriber> Layer<S> for LogCaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        use std::fmt::Write;
        use tracing::field::{Field, Visit};

        struct Visitor(String);

        impl Visit for Visitor {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    write!(self.0, "{:?}", value).ok();
                } else {
                    write!(self.0, " {}={:?}", field.name(), value).ok();
                }
            }

            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "message" {
                    self.0.push_str(value);
                } else {
                    write!(self.0, " {}={}", field.name(), value).ok();
                }
            }
        }

        let meta = event.metadata();
        let level = meta.level();
        let target = meta.target();

        let mut visitor = Visitor(String::new());
        event.record(&mut visitor);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let hh = (secs / 3600) % 24;
        let mm = (secs / 60) % 60;
        let ss = secs % 60;

        let line = format!("{hh:02}:{mm:02}:{ss:02} [{level}] {target}: {}", visitor.0);

        if let Ok(mut buf) = self.buffer.lock() {
            if buf.len() >= self.max_lines {
                buf.drain(..self.max_lines / 2);
            }
            buf.push_back(line);
        }
    }
}
