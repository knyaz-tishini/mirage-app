mod agent_api;
mod app;
mod draw;
mod runtime;
mod system_proxy;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Instant;

use softbuffer::Surface;
use tokio::sync::Mutex;
use uzor_backend_tiny_skia::TinySkiaCpuRenderContext;
use uzor::input::{InputState, MouseButton as UzorMouseButton};

use uzor_desktop::winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

use app::AppState;

fn main() {
    tracing_subscriber::fmt::init();

    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime init failed"),
    );

    let event_loop = EventLoop::new().expect("Failed to create event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut handler = MirageHandler { app: None, rt };
    event_loop.run_app(&mut handler).expect("Event loop error");
}

// =============================================================================
// Per-frame mouse accumulator
// =============================================================================

#[derive(Default)]
struct MouseAccum {
    pos: Option<(f64, f64)>,
    button_down: Option<UzorMouseButton>,
    clicked: Option<UzorMouseButton>,
}

// =============================================================================
// Main winit ApplicationHandler
// =============================================================================

struct MirageLight {
    window: Arc<Window>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    app_state: AppState,
    agent_status: Arc<Mutex<agent_api::StatusResponse>>,
    mouse: MouseAccum,
    last_frame: Instant,
    startup: Instant,
}

struct MirageHandler {
    app: Option<MirageLight>,
    rt: Arc<tokio::runtime::Runtime>,
}

impl ApplicationHandler for MirageHandler {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.app.is_some() {
            return;
        }

        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Mirage Light")
                        .with_inner_size(LogicalSize::new(780u32, 1400u32))
                        .with_resizable(false),
                )
                .expect("Failed to create window"),
        );

        let context = softbuffer::Context::new(window.clone())
            .expect("Failed to create softbuffer context");
        let surface = softbuffer::Surface::new(&context, window.clone())
            .expect("Failed to create softbuffer surface");

        let app_state = AppState::new(self.rt.clone());

        let agent_status = Arc::new(Mutex::new(agent_api::StatusResponse {
            state: "stopped".into(),
            stats: None,
        }));

        agent_api::spawn_agent_api(
            self.rt.handle(),
            app_state.agent_cmd_tx.clone(),
            agent_status.clone(),
            app_state.shared_logs.clone(),
        );

        let now = Instant::now();
        self.app = Some(MirageLight {
            window,
            surface,
            app_state,
            agent_status,
            mouse: MouseAccum::default(),
            last_frame: now,
            startup: now,
        });
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(app) = self.app.as_mut() else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => {
                app.app_state.disconnect();
                event_loop.exit();
            }

            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let dt = now.duration_since(app.last_frame).as_secs_f64();
                app.last_frame = now;
                let time = now.duration_since(app.startup).as_secs_f64();

                // Tick app state.
                app.app_state.tick(dt);

                // Update agent status (best-effort, non-blocking).
                {
                    let status_str = app.app_state.status_str().to_string();
                    let stats = app.app_state.stats.clone();
                    let agent_status = app.agent_status.clone();
                    self.rt.spawn(async move {
                        let mut lock = agent_status.lock().await;
                        lock.state = status_str;
                        lock.stats = stats;
                    });
                }

                let size = app.window.inner_size();
                let width = size.width;
                let height = size.height;

                if width == 0 || height == 0 {
                    app.window.request_redraw();
                    return;
                }

                // Resize surface.
                app.surface
                    .resize(
                        NonZeroU32::new(width).expect("width > 0"),
                        NonZeroU32::new(height).expect("height > 0"),
                    )
                    .expect("Failed to resize softbuffer surface");

                // Build InputState for coordinator.
                let mut input = InputState::new();
                input.pointer.pos = app.mouse.pos;
                input.pointer.button_down = app.mouse.button_down;
                input.pointer.clicked = app.mouse.clicked.take();
                input.dt = dt;
                input.time = time;

                // Begin coordinator frame.
                app.app_state.coordinator.begin_frame(input);

                // Render into TinySkia pixmap.
                let dpr = app.window.scale_factor();
                let mut ctx = TinySkiaCpuRenderContext::new(width, height, dpr);

                draw::draw_frame(&mut ctx, &mut app.app_state);

                // End frame — get responses and process clicks.
                let responses = app.app_state.coordinator.end_frame();

                for (id, resp) in &responses {
                    if !resp.clicked {
                        continue;
                    }
                    match id.0.as_str() {
                        "connect_btn" => {
                            match app.app_state.status {
                                runtime::ProcessStatus::Running => {
                                    app.app_state.disconnect();
                                }
                                runtime::ProcessStatus::Stopped
                                | runtime::ProcessStatus::Error(_) => {
                                    app.app_state.connect();
                                }
                                runtime::ProcessStatus::Starting => {}
                            }
                        }
                        s if s.starts_with("sel_chain_") => {
                            if let Ok(v) = s["sel_chain_".len()..].parse::<usize>() {
                                app.app_state.sel_chain = v;
                            }
                        }
                        s if s.starts_with("sel_camo_") => {
                            if let Ok(v) = s["sel_camo_".len()..].parse::<usize>() {
                                app.app_state.sel_camouflage = v;
                            }
                        }
                        s if s.starts_with("sel_shaping_") => {
                            if let Ok(v) = s["sel_shaping_".len()..].parse::<usize>() {
                                app.app_state.sel_shaping = v;
                            }
                        }
                        s if s.starts_with("sel_client_") => {
                            if let Ok(v) = s["sel_client_".len()..].parse::<usize>() {
                                app.app_state.sel_client_mode = v;
                            }
                        }
                        _ => {}
                    }
                }

                // Convert RGBA8 → softbuffer u32 (0x00RRGGBB).
                let pixels = ctx.pixels();
                let mut sb_buf = app
                    .surface
                    .buffer_mut()
                    .expect("Failed to get softbuffer buffer");

                let pixel_count = (width * height) as usize;
                for i in 0..pixel_count {
                    let base = i * 4;
                    let r = pixels[base] as u32;
                    let g = pixels[base + 1] as u32;
                    let b = pixels[base + 2] as u32;
                    sb_buf[i] = (r << 16) | (g << 8) | b;
                }

                sb_buf.present().expect("Failed to present softbuffer");
                app.window.request_redraw();
            }

            WindowEvent::CursorMoved { position, .. } => {
                let scale = app.window.scale_factor();
                app.mouse.pos = Some((position.x / scale, position.y / scale));
            }

            WindowEvent::CursorLeft { .. } => {
                app.mouse.pos = None;
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let uzor_btn = match button {
                    MouseButton::Left => UzorMouseButton::Left,
                    MouseButton::Right => UzorMouseButton::Right,
                    MouseButton::Middle => UzorMouseButton::Middle,
                    _ => return,
                };
                match state {
                    ElementState::Pressed => {
                        app.mouse.button_down = Some(uzor_btn);
                    }
                    ElementState::Released => {
                        app.mouse.button_down = None;
                        app.mouse.clicked = Some(uzor_btn);
                    }
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(app) = self.app.as_ref() {
            app.window.request_redraw();
        }
    }
}
