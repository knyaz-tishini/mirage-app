mod app;
mod platform;
mod ui;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Instant;

use uzor_desktop::winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};
use uzor_desktop::winit::dpi::LogicalSize;

use uzor::input::{InputState, MouseButton as UzorMouseButton};
use uzor::types::Rect;
use uzor_backend_tiny_skia::TinySkiaCpuRenderContext;

use softbuffer::Surface;

use app::AppState;

fn main() {
    tracing_subscriber::fmt::init();

    let event_loop = EventLoop::new().expect("Failed to create event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut handler = MirageHandler::default();
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
    scroll_y: f64,
}

// =============================================================================
// Main winit ApplicationHandler
// =============================================================================

struct MirageApp {
    window: Arc<Window>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    app_state: AppState,
    mouse: MouseAccum,
    startup: Instant,
    last_frame: Instant,
}

#[derive(Default)]
struct MirageHandler {
    app: Option<MirageApp>,
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
                        .with_title("Mirage")
                        .with_inner_size(LogicalSize::new(390u32, 700u32))
                        .with_resizable(false),
                )
                .expect("Failed to create window"),
        );

        let context = softbuffer::Context::new(window.clone())
            .expect("Failed to create softbuffer context");
        let surface = softbuffer::Surface::new(&context, window.clone())
            .expect("Failed to create softbuffer surface");

        self.app = Some(MirageApp {
            window,
            surface,
            app_state: AppState::new(),
            mouse: MouseAccum::default(),
            startup: Instant::now(),
            last_frame: Instant::now(),
        });
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(app) = self.app.as_mut() else { return };

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }

            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let dt = now.duration_since(app.last_frame).as_secs_f64();
                app.last_frame = now;
                let time = now.duration_since(app.startup).as_secs_f64();

                // Build uzor input state from accumulated mouse events
                let mut input = InputState::new();
                input.pointer.pos = app.mouse.pos;
                input.pointer.button_down = app.mouse.button_down;
                input.pointer.clicked = app.mouse.clicked.take();
                input.scroll_delta = (0.0, app.mouse.scroll_y);
                app.mouse.scroll_y = 0.0;
                input.dt = dt;
                input.time = time;

                let size = app.window.inner_size();
                let width = size.width;
                let height = size.height;

                if width == 0 || height == 0 {
                    app.window.request_redraw();
                    return;
                }

                // Resize softbuffer surface to match window
                app.surface
                    .resize(
                        NonZeroU32::new(width).expect("width > 0"),
                        NonZeroU32::new(height).expect("height > 0"),
                    )
                    .expect("Failed to resize softbuffer surface");

                let viewport = Rect::new(0.0, 0.0, width as f64, height as f64);

                // CPU render frame into TinySkia pixmap
                let dpr = app.window.scale_factor();
                let mut ctx = TinySkiaCpuRenderContext::new(width, height, dpr);
                ui::draw_frame(&mut ctx, input, viewport, &mut app.app_state);

                // Convert RGBA8 -> softbuffer u32 (0x00RRGGBB format)
                let pixels = ctx.pixels();
                let mut sb_buf = app.surface.buffer_mut().expect("Failed to get softbuffer buffer");

                let pixel_count = (width * height) as usize;
                for i in 0..pixel_count {
                    let base = i * 4;
                    let r = pixels[base] as u32;
                    let g = pixels[base + 1] as u32;
                    let b = pixels[base + 2] as u32;
                    // softbuffer expects 0x00RRGGBB (alpha ignored / always opaque)
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

            WindowEvent::MouseWheel { delta, .. } => {
                use uzor_desktop::winit::event::MouseScrollDelta;
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64 * 20.0,
                    MouseScrollDelta::PixelDelta(pos) => pos.y,
                };
                app.mouse.scroll_y += dy;
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
