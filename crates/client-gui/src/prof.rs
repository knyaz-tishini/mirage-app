//! Tiny runtime profiling control — hot-switch the render backend and scrape
//! metrics over HTTP, no relaunch needed.
//!
//!   GET  /            → {"backend":"Vello GPU","fps":221.0,"frame_ms":4.6}
//!   POST /backend/<name>  → request a backend switch (applied next frame)
//!
//! `name` ∈ vello_gpu | vello_hybrid | vello_cpu | instanced_wgpu | urx_wgpu |
//! urx_wgpu_full | urx_hybrid | urx_cpu | tiny_skia. The app thread reads
//! `requested` each frame, calls `render_control.set_backend`, and writes the
//! live fps/frame_ms/backend back here.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use uzor::platform::types::RenderBackend;

#[derive(Default)]
pub struct ProfState {
    /// Backend name requested over HTTP, consumed by the app thread.
    pub requested: Option<String>,
    pub fps: f32,
    pub frame_ms: f32,
    pub backend: String,
}

pub type ProfHandle = Arc<Mutex<ProfState>>;

/// Map a backend name to a [`RenderBackend`].
pub fn parse_backend(name: &str) -> Option<RenderBackend> {
    match name.trim() {
        "vello_gpu" => Some(RenderBackend::VelloGpu),
        "vello_hybrid" => Some(RenderBackend::VelloHybrid),
        "vello_cpu" => Some(RenderBackend::VelloCpu),
        "instanced_wgpu" => Some(RenderBackend::InstancedWgpu),
        "urx_wgpu" => Some(RenderBackend::UrxWgpu),
        "urx_wgpu_full" => Some(RenderBackend::UrxWgpuFull),
        "urx_hybrid" => Some(RenderBackend::UrxHybrid),
        "urx_cpu" => Some(RenderBackend::UrxCpu),
        "tiny_skia" => Some(RenderBackend::TinySkia),
        _ => None,
    }
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
                    state.lock().unwrap().requested = Some(name);
                }
                format!("{{\"ok\":{ok}}}")
            } else {
                let st = state.lock().unwrap();
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
