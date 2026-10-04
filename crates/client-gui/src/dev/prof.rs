//! Tiny runtime profiling control — hot-switch the render backend and scrape
//! metrics over HTTP, no relaunch needed. Dev-tools only.
//!
//!   GET  /            → {"backend":"Vello GPU","fps":221.0,"frame_ms":4.6}
//!   POST /backend/<name>  → request a backend switch (applied next frame)
//!
//! `name` is anything [`crate::render_backend::parse_backend`] accepts. The app
//! thread calls [`tick`] each frame: it reads `requested`, calls
//! `render_control.set_backend`, and writes the live fps/frame_ms/backend back
//! here.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use uzor::framework::render_control::RenderControl;

use crate::render_backend::parse_backend;

#[derive(Default)]
pub struct ProfState {
    /// Backend name requested over HTTP, consumed by the app thread.
    pub requested: Option<String>,
    pub fps: f32,
    pub frame_ms: f32,
    pub backend: String,
}

pub type ProfHandle = Arc<Mutex<ProfState>>;

fn lock(state: &ProfHandle) -> MutexGuard<'_, ProfState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Per-frame sync with the app thread: publish the live render stats, apply a
/// pending backend switch, and return the one-line HUD readout
/// (`<backend>  <fps>fps  <ms>ms`).
pub fn tick(state: &ProfHandle, render_control: &mut dyn RenderControl) -> String {
    let backend = render_control.active_backend().label().to_string();
    let fps = render_control.measured_fps();
    let frame_ms = render_control.last_frame_time_ms();
    let requested = {
        let mut p = lock(state);
        p.fps = fps;
        p.frame_ms = frame_ms;
        p.backend.clone_from(&backend);
        p.requested.take()
    };
    if let Some(b) = requested.as_deref().and_then(parse_backend) {
        render_control.set_backend(b);
    }
    format!("{backend}  {fps:.0}fps  {frame_ms:.1}ms")
}

/// Spawn the control server on `127.0.0.1:port`.
pub fn serve(state: ProfHandle, port: u16) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("prof server bind :{port} failed: {e}");
                return;
            }
        };
        tracing::info!("[prof] control on http://127.0.0.1:{port}");
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 1024];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let mut head = req.lines().next().unwrap_or("").split_whitespace();
            let method = head.next().unwrap_or("");
            let path = head.next().unwrap_or("");

            let body = if method == "POST" && path.starts_with("/backend/") {
                let name = path.trim_start_matches("/backend/").to_string();
                let ok = parse_backend(&name).is_some();
                if ok {
                    lock(&state).requested = Some(name);
                }
                format!("{{\"ok\":{ok}}}")
            } else {
                let st = lock(&state);
                format!(
                    "{{\"backend\":\"{}\",\"fps\":{:.1},\"frame_ms\":{:.2}}}",
                    st.backend, st.fps, st.frame_ms
                )
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
}
