//! MirageClientApp — uzor App<NoPanel> implementation.
//!
//! Skin switching:
//!   Key 1  → Bloom
//!   Key 2  → Planet
//!   Key 3  → Radar
//!   Tab    → cycle (Bloom → Planet → Radar → Bloom)
//!
//! Center-tap → toggle connect / disconnect (entire area below titlebar).
//! Menu (M button or keyboard M) → bottom-sheet overlay with skin switcher.
//! Click routing goes through the uzor LayoutManager so `/input/click` works.

use std::time::Instant;

use uzor::core::types::Rect;
use uzor::framework::app::{App, NoPanel};
use uzor::framework::multi_window::WindowCtx;
use uzor::framework::widgets::lm;
use uzor::input::core::event_processor::PlatformEvent;
use uzor::input::keyboard::events::KeyCode;
use uzor::types::unsafe_widget_id;

use crate::centerpiece::{make_skin, CenterpieceState, ConnPhase, Skin, SkinKind, SkinRect};
use crate::shell::desktop::{draw_titlebar, TitlebarHover, TITLEBAR_H};
use crate::shell::menu::{draw_menu_sheet, SHEET_HEIGHT};

// ── Widget ids ─────────────────────────────────────────────────────────────────

/// Hit-zone covering the entire centerpiece area (connect/disconnect toggle).
fn id_centerpiece() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:centerpiece")
}

/// Hit-zone over the M glyph in the titlebar.
fn id_menu_btn() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:menu_btn")
}

/// Full-window scrim behind the bottom sheet (click closes the menu).
fn id_scrim() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:scrim")
}

/// Skin-row hit-zone ids.
fn id_skin_bloom() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:skin:bloom")
}
fn id_skin_planet() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:skin:planet")
}
fn id_skin_radar() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:skin:radar")
}

// ── App state ─────────────────────────────────────────────────────────────────

/// Main application state.
pub struct MirageClientApp {
    skin: Box<dyn Skin>,
    skin_kind: SkinKind,
    current_phase: ConnPhase,

    // Pending phase transitions (mock — no live backend).
    pending_connect_at: Option<Instant>,
    pending_disconnect_at: Option<Instant>,

    // Shell state.
    menu_open: bool,
    hover: TitlebarHover,

    // Frame timing.
    last_frame: Instant,
}

impl MirageClientApp {
    /// Construct with an explicit initial skin + connection state (used by the
    /// `--skin` / `--connected` CLI flags for deterministic screenshot capture).
    pub fn with_skin(skin_kind: SkinKind, connected: bool) -> Self {
        Self {
            skin: make_skin(skin_kind),
            skin_kind,
            current_phase: if connected { ConnPhase::Connected } else { ConnPhase::Disconnected },
            pending_connect_at: None,
            pending_disconnect_at: None,
            menu_open: false,
            hover: TitlebarHover::default(),
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
            }
        }
        if let Some(at) = self.pending_disconnect_at {
            if at.elapsed().as_millis() >= 400 {
                self.current_phase = ConnPhase::Disconnected;
                self.pending_disconnect_at = None;
            }
        }

        let w = win.rect.width;
        let h = win.rect.height;

        // ── Process layout clicks BEFORE painting ─────────────────────────────
        // was_clicked() reads the previous frame's click state from the
        // coordinator — must be queried before build() registers new nodes.

        // Menu button always processes (opens/closes the sheet).
        if win.layout.was_clicked(&id_menu_btn()) {
            self.menu_open = !self.menu_open;
        }

        if self.menu_open {
            // Scrim click → close menu.
            if win.layout.was_clicked(&id_scrim()) {
                self.menu_open = false;
            }
            // Skin-row clicks — switch skin and close menu.
            if win.layout.was_clicked(&id_skin_bloom()) {
                self.switch_skin(SkinKind::Bloom);
                self.menu_open = false;
            }
            if win.layout.was_clicked(&id_skin_planet()) {
                self.switch_skin(SkinKind::Planet);
                self.menu_open = false;
            }
            if win.layout.was_clicked(&id_skin_radar()) {
                self.switch_skin(SkinKind::Radar);
                self.menu_open = false;
            }
        } else {
            // Centerpiece toggle only when menu is closed.
            if win.layout.was_clicked(&id_centerpiece()) {
                self.toggle_connection();
            }
        }

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

        // ── Bottom-sheet menu overlay (when open) ─────────────────────────────
        if self.menu_open {
            draw_menu_sheet(render, w, h, self.skin_kind);
        }

        // ── Register hit-zones with the LayoutManager ─────────────────────────
        // Hit-zones are transparent buttons that register CLICK sense so both
        // real mouse events and `/input/click` agent commands reach them.
        {
            let layout = &mut *win.layout;
            let render = &mut *win.render;

            if self.menu_open {
                // ── Menu open: scrim + sheet rows ─────────────────────────────
                // Register scrim FIRST (bottom z-order in the hit stack).
                // The sheet skin-row buttons are registered AFTER so they win.
                let scrim_rect = Rect {
                    x: 0.0,
                    y: 0.0,
                    width: w,
                    height: h,
                };
                lm::button(id_scrim(), scrim_rect)
                    .build(layout, render);

                // Sheet occupies the bottom SHEET_HEIGHT pixels.
                let sheet_y = h - SHEET_HEIGHT;
                // Row layout: skip a header area of 40 px, then 3 rows × 48 px.
                let row_x = 0.0;
                let row_w = w;
                let row_h = 48.0;
                let rows_start = sheet_y + 40.0;

                lm::button(id_skin_bloom(), Rect {
                    x: row_x,
                    y: rows_start,
                    width: row_w,
                    height: row_h,
                }).build(layout, render);

                lm::button(id_skin_planet(), Rect {
                    x: row_x,
                    y: rows_start + row_h,
                    width: row_w,
                    height: row_h,
                }).build(layout, render);

                lm::button(id_skin_radar(), Rect {
                    x: row_x,
                    y: rows_start + row_h * 2.0,
                    width: row_w,
                    height: row_h,
                }).build(layout, render);
            } else {
                // ── Menu closed: normal centerpiece + menu button ─────────────

                // Centerpiece hit-zone: full area below the titlebar.
                let centerpiece_rect = Rect {
                    x: 0.0,
                    y: TITLEBAR_H,
                    width: w,
                    height: (h - TITLEBAR_H).max(0.0),
                };
                lm::button(id_centerpiece(), centerpiece_rect)
                    .build(layout, render);
            }

            // Menu-button hit-zone always registered (opens/closes the sheet).
            // Registered last so it wins over the centerpiece below it.
            let menu_rect = Rect {
                x: 4.0,
                y: 0.0,
                width: 44.0,
                height: TITLEBAR_H,
            };
            lm::button(id_menu_btn(), menu_rect)
                .build(layout, render);
        }
    }

    /// Handle raw platform events for keyboard input only.
    ///
    /// Pointer clicks are handled via the LayoutManager hit-zones in `ui()`.
    fn on_event(&mut self, event: &PlatformEvent) -> bool {
        match event {
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
