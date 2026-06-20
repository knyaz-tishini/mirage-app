//! MirageClientApp — uzor App<NoPanel> implementation.
//!
//! Skin switching:
//!   Key 1  → Bloom
//!   Key 2  → Planet
//!   Key 3  → Radar
//!   Tab    → cycle (Bloom → Planet → Radar → Bloom)
//!
//! Center-tap → toggle connect / disconnect.

use std::time::Instant;

use uzor::framework::app::{App, NoPanel};
use uzor::framework::multi_window::WindowCtx;
use uzor::input::core::event_processor::PlatformEvent;
use uzor::input::keyboard::events::KeyCode;
use uzor::input::pointer::state::MouseButton;
use uzor::render::{TextAlign, TextBaseline};

use crate::centerpiece::{make_skin, CenterpieceState, ConnPhase, Skin, SkinKind, SkinRect};
use crate::gesture::{TapDetector, TapEvent};
use crate::shell::desktop::{
    draw_titlebar, is_menu_btn, TitlebarHover, TITLEBAR_H,
};
use crate::toast::ToastState;

// ── App state ─────────────────────────────────────────────────────────────────

/// Main application state.
pub struct MirageClientApp {
    skin: Box<dyn Skin>,
    skin_kind: SkinKind,
    current_phase: ConnPhase,

    // Pending phase transitions (mock — no live backend).
    pending_connect_at: Option<Instant>,
    pending_disconnect_at: Option<Instant>,

    // Gesture / input.
    gesture: TapDetector,
    last_mouse_pos: (f64, f64),

    // Shell state.
    menu_open: bool,
    hover: TitlebarHover,

    // Toast.
    toast: ToastState,

    // Frame timing.
    last_frame: Instant,
}

impl MirageClientApp {
    pub fn new() -> Self {
        Self::with_skin(SkinKind::Bloom, false)
    }

    /// Construct with an explicit initial skin + connection state (used by the
    /// `--skin` / `--connected` CLI flags for deterministic screenshot capture).
    pub fn with_skin(skin_kind: SkinKind, connected: bool) -> Self {
        Self {
            skin: make_skin(skin_kind),
            skin_kind,
            current_phase: if connected { ConnPhase::Connected } else { ConnPhase::Disconnected },
            pending_connect_at: None,
            pending_disconnect_at: None,
            gesture: TapDetector::new(),
            last_mouse_pos: (0.0, 0.0),
            menu_open: false,
            hover: TitlebarHover::default(),
            toast: ToastState::new(),
            last_frame: Instant::now(),
        }
    }

    // ── Skin switching ────────────────────────────────────────────────────────

    fn switch_skin(&mut self, kind: SkinKind) {
        if self.skin_kind == kind {
            return;
        }
        self.skin_kind = kind;
        self.skin = make_skin(kind);
        let label = match kind {
            SkinKind::Bloom  => "Bloom",
            SkinKind::Planet => "Planet",
            SkinKind::Radar  => "Radar",
        };
        self.toast.show(label);
    }

    fn cycle_skin(&mut self) {
        let next = self.skin_kind.next();
        self.switch_skin(next);
    }

    // ── Connection toggle ─────────────────────────────────────────────────────

    fn toggle_connection(&mut self) {
        match self.current_phase {
            ConnPhase::Disconnected => {
                self.current_phase = ConnPhase::Connecting;
                self.pending_connect_at = Some(Instant::now());
            }
            ConnPhase::Connected => {
                self.current_phase = ConnPhase::Disconnecting;
                self.pending_disconnect_at = Some(Instant::now());
            }
            ConnPhase::Connecting | ConnPhase::Disconnecting => {}
        }
    }
}

// ── App trait ─────────────────────────────────────────────────────────────────

impl App<NoPanel> for MirageClientApp {
    /// Called every frame for every window. Paints the full UI into `win.render`.
    fn ui(&mut self, win: &mut WindowCtx<'_, NoPanel>) {
        // ── Timing ────────────────────────────────────────────────────────────
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f64().min(0.1);
        self.last_frame = now;

        // ── Advance mock phase transitions ────────────────────────────────────
        if let Some(at) = self.pending_connect_at {
            if at.elapsed().as_millis() >= 700 {
                self.current_phase = ConnPhase::Connected;
                self.pending_connect_at = None;
                self.toast.show("Connected");
            }
        }
        if let Some(at) = self.pending_disconnect_at {
            if at.elapsed().as_millis() >= 400 {
                self.current_phase = ConnPhase::Disconnected;
                self.pending_disconnect_at = None;
                self.toast.show("Disconnected");
            }
        }

        let w = win.rect.width;
        let h = win.rect.height;
        let render = &mut *win.render;

        // ── Background ────────────────────────────────────────────────────────
        render.set_fill_color("#0B0B0E");
        render.fill_rect(0.0, 0.0, w, h);

        // ── Skin (centerpiece) ────────────────────────────────────────────────
        let skin_rect = SkinRect {
            x: 0.0,
            y: TITLEBAR_H,
            w,
            h: h - TITLEBAR_H,
        };
        let state = CenterpieceState {
            dt,
            phase: self.current_phase,
            rect: skin_rect,
        };
        self.skin.render(render, state);

        // ── Titlebar chrome ───────────────────────────────────────────────────
        draw_titlebar(render, self.hover, w, self.menu_open);

        let phase_str = match self.current_phase {
            ConnPhase::Disconnected  => "OFFLINE",
            ConnPhase::Connecting    => "CONNECTING\u{2026}",
            ConnPhase::Connected     => "CONNECTED",
            ConnPhase::Disconnecting => "DISCONNECTING\u{2026}",
        };
        let phase_color = match self.current_phase {
            ConnPhase::Connected                              => "#7FD6A0",
            ConnPhase::Connecting | ConnPhase::Disconnecting => "#FBB26A",
            ConnPhase::Disconnected                          => "#878B91",
        };
        render.set_fill_color(phase_color);
        render.set_font("9px DM Mono");
        render.set_text_align(TextAlign::Center);
        render.set_text_baseline(TextBaseline::Middle);
        render.fill_text(phase_str, w / 2.0, TITLEBAR_H / 2.0);

        // ── Toast ─────────────────────────────────────────────────────────────
        if let Some(msg) = self.toast.current() {
            let alpha = self.toast.alpha();
            render.set_global_alpha(alpha);
            render.set_fill_color("#1C1D23");
            render.fill_rect(w * 0.15, h - 44.0, w * 0.70, 28.0);
            render.set_fill_color("#F4F4F5");
            render.set_font("11px DM Mono");
            render.set_text_align(TextAlign::Center);
            render.set_text_baseline(TextBaseline::Middle);
            render.fill_text(msg, w / 2.0, h - 30.0);
            render.set_global_alpha(1.0);
        }
    }

    /// Handle raw platform events for pointer + keyboard input.
    fn on_event(&mut self, event: &PlatformEvent) -> bool {
        match event {
            PlatformEvent::PointerMoved { x, y } => {
                // Window width is not known here; use last_mouse_pos for x
                // and defer is_minimize/is_close to the stored window width
                // via a sentinel — instead, track and recompute at ui() time.
                // For titlebar hit-testing we need w; approximate from
                // a large value so the right-side buttons still register.
                // The actual w is updated each frame in ui(); using 9999 here
                // means the right-side zone comparison works correctly for any
                // real window width because `is_close` checks `x >= w - BTN_W`.
                // We store the mouse pos and recompute hover in ui() on next frame.
                self.last_mouse_pos = (*x, *y);
                false
            }

            PlatformEvent::PointerDown { x, y, button: MouseButton::Left } => {
                self.last_mouse_pos = (*x, *y);
                self.gesture.on_pointer_down(*x, *y);
                false
            }

            PlatformEvent::PointerUp { x, y, button: MouseButton::Left } => {
                self.last_mouse_pos = (*x, *y);

                if is_menu_btn(*x, *y) {
                    self.menu_open = !self.menu_open;
                    return true;
                }

                if *y >= TITLEBAR_H {
                    if let Some(TapEvent::Single) = self.gesture.on_pointer_up(*x, *y) {
                        // Compute skin_rect based on approximate size — will be
                        // refined once we get rect from ui(). Use generous h.
                        let skin_rect = SkinRect {
                            x: 0.0,
                            y: TITLEBAR_H,
                            w: 380.0,
                            h: 504.0,
                        };
                        if self.skin.hit_center(*x, *y, skin_rect) {
                            self.toggle_connection();
                            return true;
                        }
                    }
                }
                false
            }

            PlatformEvent::KeyDown { key, .. } => {
                match key {
                    KeyCode::Num1 => { self.switch_skin(SkinKind::Bloom); true }
                    KeyCode::Num2 => { self.switch_skin(SkinKind::Planet); true }
                    KeyCode::Num3 => { self.switch_skin(SkinKind::Radar); true }
                    KeyCode::Tab  => { self.cycle_skin(); true }
                    _             => false,
                }
            }

            _ => false,
        }
    }
}
