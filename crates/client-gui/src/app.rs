//! MirageClientApp — uzor App<NoPanel> implementation.
//!
//! Skin switching:
//!   Key 1  → Bloom
//!   Key 2  → Planet
//!   Key 3  → Radar
//!   Tab    → cycle (Bloom → Planet → Radar → Bloom)
//!
//! Center-tap → toggle connect / disconnect (entire area below chrome strip).
//! Menu (chrome M button or keyboard M) → bottom-sheet overlay with skin switcher.
//! Drag / minimize / close → handled by the uzor chrome path (WindowHost).

use std::time::Instant;

use uzor::core::types::Rect;
use uzor::framework::app::{App, NoPanel};
use uzor::framework::multi_window::WindowCtx;
use uzor::render::{TextAlign, TextBaseline};
use uzor::framework::widgets::lm;
use uzor::input::core::event_processor::PlatformEvent;
use uzor::input::keyboard::events::KeyCode;
use uzor::input::{Sense, WidgetKind};
use uzor::layout::{ChromeWindowControl, LayoutManager, LayoutNodeId};
use uzor::ui::widgets::composite::chrome::theme::ChromeTheme;
use uzor::ui::effects::text::{GlitchGlyph, GlitchStyle};
use uzor::types::unsafe_widget_id;

use crate::centerpiece::{make_skin, CenterpieceState, ConnPhase, Skin, SkinKind, SkinRect};
use crate::shell::menu::{draw_menu_overlay, menu_height, menu_rows_start, MENU_ROW_H};

// ── Chrome strip height ────────────────────────────────────────────────────────
// Matches DefaultChromeStyle::chrome_height() == 32.0.  The skin rect starts
// immediately below this strip.
const CHROME_H: f64 = 32.0;

// ── Dark chrome theme ──────────────────────────────────────────────────────────

/// Minimal dark theme that matches the app's #0B0B0E background.
struct DarkChromeTheme;

impl ChromeTheme for DarkChromeTheme {
    fn background(&self)      -> &str { "#0C0C10" }
    fn icon_normal(&self)     -> &str { "#555860" }
    fn icon_hover(&self)      -> &str { "#F4F4F5" }
    fn button_hover(&self)    -> &str { "rgba(255,255,255,0.07)" }
    fn close_hover(&self)     -> &str { "rgba(224,86,86,0.6)" }
    fn separator(&self)       -> &str { "rgba(255,255,255,0.08)" }
    fn tab_bg_normal(&self)   -> &str { "transparent" }
    fn tab_bg_hover(&self)    -> &str { "rgba(255,255,255,0.07)" }
    fn tab_bg_active(&self)   -> &str { "#0C0C10" }
    fn tab_text_normal(&self) -> &str { "#555860" }
    fn tab_text_hover(&self)  -> &str { "#F4F4F5" }
    fn tab_text_active(&self) -> &str { "#F4F4F5" }
    fn tab_accent(&self)      -> &str { "#FBB26A" }
    fn drag_zone_bg(&self)    -> &str { "transparent" }
    fn tooltip_bg(&self)      -> &str { "#1A1A22" }
    fn tooltip_text(&self)    -> &str { "#F4F4F5" }
}

// ── Widget ids ─────────────────────────────────────────────────────────────────

/// Hit-zone covering the entire centerpiece area (connect/disconnect toggle).
fn id_centerpiece() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:centerpiece")
}

/// Scrim behind the overlay (click closes the menu).
fn id_scrim() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:scrim")
}

/// Menu panel body — absorbs clicks so panel-background taps don't dismiss.
fn id_menu_panel() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:menu_panel")
}

/// Register an INVISIBLE click hit-zone (no paint). Unlike `lm::button` — which
/// paints a hover background over its whole rect — this only adds the rect to
/// the input coordinator so `was_clicked` resolves, leaving the scene untouched.
fn register_hit(layout: &mut LayoutManager<NoPanel>, id: uzor::types::WidgetId, rect: Rect) {
    let layer = layout.compute_layer_for(LayoutNodeId::ROOT);
    layout
        .ctx_mut()
        .input
        .register_atomic(id, WidgetKind::Button, rect, Sense::CLICK, &layer);
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

    // Pointer gesture state (drag-rotate / tap / double-tap / wheel).
    /// Left button currently held (tracked from raw on_event).
    pointer_down: bool,
    /// Last raw pointer position (physical px) for drag deltas.
    last_phys: Option<(f64, f64)>,
    /// Accumulated travel during the current press — distinguishes drag vs tap.
    drag_moved: f64,
    /// Deferred single-tap timestamp (ms) — fires connect if no second tap
    /// arrives within the double-click window.
    pending_tap_at: Option<f64>,
    /// Timestamp (ms) of the last empty tap, for double-click discrimination.
    last_tap_ms: f64,

    // Glitchy ASCII "M" menu glyph.
    menu_glyph: GlitchGlyph,
    menu_style: GlitchStyle,
    /// Eased glitch intensity (hover) + a decaying click pulse.
    menu_glitch: f64,
    menu_pulse: f64,

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
            pointer_down: false,
            last_phys: None,
            drag_moved: 0.0,
            pending_tap_at: None,
            last_tap_ms: -1.0e9,
            menu_glyph: GlitchGlyph::letter_m(),
            menu_style: GlitchStyle::default(),
            menu_glitch: 0.0,
            menu_pulse: 0.0,
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

    /// Single tap on empty space → connect (only from a settled disconnected
    /// state). Grid → planet, off → bloom, etc.
    fn request_connect(&mut self) {
        if matches!(self.current_phase, ConnPhase::Disconnected) {
            self.current_phase = ConnPhase::Connecting;
            self.pending_connect_at = Some(Instant::now());
        }
    }

    /// Double tap → disconnect (only when connected). Planet → grid.
    fn request_disconnect(&mut self) {
        if matches!(self.current_phase, ConnPhase::Connected) {
            self.current_phase = ConnPhase::Disconnecting;
            self.pending_disconnect_at = Some(Instant::now());
            self.skin.clear_selection();
        }
    }
}

/// Double-click window in ms.
const DBLCLICK_MS: f64 = 280.0;
/// Pointer travel (logical px) above which a press+release is a drag, not a tap.
const TAP_SLOP: f64 = 7.0;

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

        // ── Glitch "M" intensity: hover over the menu button + click pulse ─────
        let hover_menu = win
            .layout
            .pointer_pos()
            .map(|(px, py)| px >= 0.0 && px < 36.0 && py >= 0.0 && py < CHROME_H)
            .unwrap_or(false);
        let target = if hover_menu { 1.0 } else { 0.0 };
        self.menu_glitch += (target - self.menu_glitch) * (8.0 * dt).min(1.0);
        self.menu_pulse = (self.menu_pulse - dt * 2.5).max(0.0);
        let glitch_amt = self.menu_glitch.max(self.menu_pulse);
        self.menu_glyph.update(dt, glitch_amt, &self.menu_style);

        // ── Process layout clicks BEFORE painting ─────────────────────────────
        // was_clicked() reads the previous frame's click state from the
        // coordinator — must be queried before build() registers new nodes.

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
            // ── Tap / double-tap on the centerpiece (menu closed) ──────────────
            // Drag-rotate and wheel-zoom are handled in `on_event` (raw pointer
            // stream); here we only resolve discrete taps via the layout. A tap
            // is a press+release that barely moved (`drag_moved < TAP_SLOP`).
            let now_ms = win.layout.frame_time_ms();
            if win.layout.was_clicked(&id_centerpiece()) && self.drag_moved < TAP_SLOP {
                let (tx, ty) = win.layout.pointer_pos().unwrap_or((0.0, 0.0));
                // Let the skin consume it first (e.g. node → info popup).
                if !self.skin.tap(tx, ty) {
                    if now_ms - self.last_tap_ms < DBLCLICK_MS {
                        // Double tap → disconnect (cancel the pending single).
                        self.pending_tap_at = None;
                        self.request_disconnect();
                    } else {
                        // Provisional single tap — defer to disambiguate.
                        self.pending_tap_at = Some(now_ms);
                    }
                    self.last_tap_ms = now_ms;
                }
            }
            // Fire the deferred single tap once the double-click window passes.
            if let Some(at) = self.pending_tap_at {
                if now_ms - at >= DBLCLICK_MS {
                    self.pending_tap_at = None;
                    self.request_connect();
                }
            }
        }

        let render = &mut *win.render;

        // ── Background ────────────────────────────────────────────────────────
        render.set_fill_color("#0B0B0E");
        render.fill_rect(0.0, 0.0, w, h);

        // ── Skin (centerpiece) ────────────────────────────────────────────────
        let skin_rect = SkinRect {
            x: 0.0,
            y: CHROME_H,
            w,
            h: h - CHROME_H,
        };
        let state = CenterpieceState {
            dt,
            phase: self.current_phase,
            rect: skin_rect,
        };
        self.skin.render(render, state);

        // ── Top overlay menu (when open) ──────────────────────────────────────
        if self.menu_open {
            draw_menu_overlay(render, w, h, self.skin_kind);
        }

        // ── Chrome strip ──────────────────────────────────────────────────────
        // Renders last so it paints on top of the centerpiece and menu sheet.
        // Registering the chrome slot enables handle_chrome_press → WindowHost
        // for drag, minimize, and close automatically.
        {
            let layout = &mut *win.layout;
            let render = &mut *win.render;

            lm::chrome()
                .show_menu_btn(true)
                .menu_left(true)        // M on the left edge
                .show_maximize(false)   // minimize + close only (no maximize)
                .tabs(&[])
                .theme(Box::new(DarkChromeTheme))
                .build(layout, render);

            if self.menu_open {
                // ── Menu open: scrim → panel → skin rows ──────────────────────
                // Hit priority follows registration order (later wins). Scrim
                // first (dismiss fallback), then the panel body (absorbs taps
                // on the overlay background), then the skin rows on top.
                // All invisible — the sheet itself is painted in draw_menu_overlay.
                register_hit(layout, id_scrim(), Rect {
                    x: 0.0,
                    y: CHROME_H,
                    width: w,
                    height: (h - CHROME_H).max(0.0),
                });
                register_hit(layout, id_menu_panel(), Rect {
                    x: 0.0,
                    y: CHROME_H,
                    width: w,
                    height: menu_height(),
                });

                let row_h = MENU_ROW_H;
                let rows_start = menu_rows_start();
                register_hit(layout, id_skin_bloom(), Rect { x: 0.0, y: rows_start, width: w, height: row_h });
                register_hit(layout, id_skin_planet(), Rect { x: 0.0, y: rows_start + row_h, width: w, height: row_h });
                register_hit(layout, id_skin_radar(), Rect { x: 0.0, y: rows_start + row_h * 2.0, width: w, height: row_h });
            } else {
                // ── Menu closed: invisible centerpiece hit-zone ───────────────
                register_hit(layout, id_centerpiece(), Rect {
                    x: 0.0,
                    y: CHROME_H,
                    width: w,
                    height: (h - CHROME_H).max(0.0),
                });
            }
        }

        // ── Glitchy ASCII "M" over the menu button slot ───────────────────────
        // Painted last, over the chrome's default hamburger (covered first).
        {
            let render = &mut *win.render;
            const SLOT_W: f64 = 36.0;
            render.set_fill_color("#0C0C10");
            render.fill_rect(0.0, 0.0, SLOT_W, CHROME_H);

            let cols = self.menu_glyph.cols();
            let rows = self.menu_glyph.rows();
            let pad_x = 8.0;
            let pad_y = 5.0;
            let cw = (SLOT_W - pad_x * 2.0) / cols as f64;
            let ch = (CHROME_H - pad_y * 2.0) / rows as f64;
            let fs = (ch + 1.5).max(5.0);
            render.set_font(&format!("{}px monospace", fs as i32));
            render.set_text_align(TextAlign::Left);
            render.set_text_baseline(TextBaseline::Top);
            for r in 0..rows {
                for c in 0..cols {
                    let cell = self.menu_glyph.cell(c, r);
                    if !cell.on {
                        continue;
                    }
                    render.set_fill_color(&format!(
                        "rgb({},{},{})",
                        cell.color[0], cell.color[1], cell.color[2]
                    ));
                    render.fill_text(
                        &cell.ch.to_string(),
                        pad_x + c as f64 * cw,
                        pad_y + r as f64 * ch,
                    );
                }
            }
        }
    }

    /// Chrome control buttons (menu / minimize / close-app) dispatched here
    /// by the framework after `route_click` processes the chrome hit.
    fn on_chrome_control(
        &mut self,
        _layout: &mut LayoutManager<NoPanel>,
        control: ChromeWindowControl,
    ) {
        match control {
            ChromeWindowControl::Menu => {
                self.menu_open = !self.menu_open;
                self.menu_pulse = 1.0; // glitch burst on click
            }
            ChromeWindowControl::CloseApp => {
                std::process::exit(0);
            }
            // Minimize and MaximizeRestore are handled directly by
            // handle_chrome_press → WindowHost (set_minimized / set_maximized).
            // CloseWindow is a no-op for a single-window app.
            _ => {}
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
            // Drag-rotate: feed raw pointer deltas to the active skin while the
            // button is held (planet rotates; bloom/radar ignore). Coordinates
            // here are physical px — fine for a rotation sensitivity factor.
            PlatformEvent::PointerDown { .. } => {
                self.pointer_down = true;
                self.drag_moved = 0.0;
                false
            }
            PlatformEvent::PointerUp { .. } => {
                self.pointer_down = false;
                false
            }
            PlatformEvent::PointerMoved { x, y } => {
                if let Some((lx, ly)) = self.last_phys {
                    if self.pointer_down && !self.menu_open {
                        self.skin.drag(x - lx, y - ly);
                        self.drag_moved += (x - lx).abs() + (y - ly).abs();
                    }
                }
                self.last_phys = Some((*x, *y));
                false
            }
            // Wheel → zoom (planet).
            PlatformEvent::Scroll { dy, .. } => {
                if !self.menu_open {
                    self.skin.zoom(*dy);
                }
                false
            }
            _ => false,
        }
    }
}
