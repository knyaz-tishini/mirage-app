//! MirageClientApp — uzor App<NoPanel> implementation.
//!
//! Skin switching:
//!   Key 1  → Bloom
//!   Key 2  → Planet
//!   Tab    → cycle (Bloom → Planet → Bloom)
//!
//! Center-tap toggles connect / disconnect. A second tap does not wait.
//! Planet is the exception: a double-tap closes the open node, because a
//! single click there rotates the globe.
//! Menu (chrome M button or keyboard M) → bottom-sheet overlay with skin switcher.
//! Drag / minimize / close → handled by the uzor chrome path (WindowHost).

use std::time::Instant;

use uzor::core::types::Rect;
use uzor::framework::app::{App, NoPanel};
use uzor::framework::multi_window::WindowCtx;
use uzor::render::{TextAlign, TextBaseline};
use uzor::framework::widgets::lm;
use uzor::input::core::event_processor::{ImeEvent, PlatformEvent};
use uzor::input::keyboard::events::KeyCode;
use uzor::input::ModifierKeys;
use uzor::input::{Sense, WidgetKind};
use uzor::layout::{ChromeWindowControl, LayoutManager, LayoutNodeId};
use uzor::ui::widgets::composite::chrome::theme::ChromeTheme;
use uzor::types::unsafe_widget_id;

use crate::centerpiece::{make_skin, CenterpieceState, ConnPhase, Skin, SkinKind, SkinRect};
use crate::shell::menu::{self, ListPart, MenuAction, MenuRow, MenuTab};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListEditor {
    Sites,
    Apps,
}

/// Customer cabinet in the machine browser. No token, no account number:
/// a new tab has an empty sessionStorage and shows the account-number form.
const CABINET_URL: &str = "https://m1r4g3.bid/cabinet/";

fn open_cabinet() {
    if !crate::backend::cabinet_marker_present() {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", CABINET_URL])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(CABINET_URL).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(CABINET_URL).spawn();
    }
}

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
    fn close_icon_hover(&self) -> &str { "#F4F4F5" }
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

/// Per-menu-row hit-zone id (data-driven menu).
fn id_menu_row(i: usize) -> uzor::types::WidgetId {
    unsafe_widget_id(&format!("mirage:menu:row:{i}"))
}
/// Per-menu-tab hit-zone id.
fn id_menu_tab(i: usize) -> uzor::types::WidgetId {
    unsafe_widget_id(&format!("mirage:menu:tab:{i}"))
}

fn id_ticker() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:ticker")
}

fn id_log() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:log")
}

fn id_restart() -> uzor::types::WidgetId {
    unsafe_widget_id("mirage:restart")
}

/// The update control sits just under the chrome, on top of the menu, so a
/// ready install stays visible while Setup is open.
fn restart_rect(w: f64) -> Rect {
    Rect { x: 12.0, y: CHROME_H + 6.0, width: (w - 24.0).max(0.0), height: 28.0 }
}

// ── App state ─────────────────────────────────────────────────────────────────

/// Main application state.
pub struct MirageClientApp {
    skin: Box<dyn Skin>,
    skin_kind: SkinKind,
    current_phase: ConnPhase,

    /// When the optimistic "Connecting" phase began; it is abandoned if the
    /// tunnel has not come up within the give-up window.
    pending_connect_at: Option<Instant>,

    // Shell state.
    menu_open: bool,
    /// Active menu tab.
    menu_tab: MenuTab,
    /// Menu panel height as a fraction of the area below chrome (resizable).
    menu_frac: f64,
    /// Content scroll offset (px) within the active tab.
    menu_scroll: f64,
    /// Scroll inside the Manual country grid. Independent of `menu_scroll`.
    country_scroll: f64,
    country_drag: bool,
    country_grab: f64,
    country_dragged: bool,
    country_view: Option<menu::CountryScroll>,
    /// Logical pointer from the previous frame, for wheel-over-grid.
    last_pointer: Option<(f64, f64)>,
    /// Whether the bottom separator is being dragged.
    menu_drag_sep: bool,
    /// Last pointer Y (logical) for separator-drag deltas.
    menu_last_y: f64,
    /// Pointer-down state at the previous frame (for press-edge detection).
    prev_pointer_down: bool,
    /// Chosen exit country. Honored only when `exit_auto` is false.
    selected_exit: Option<String>,
    /// Exit countries from the cabinet catalog. Manual mode lists these.
    countries: Vec<String>,
    /// Auto lets the entrance pick. Manual lists `countries`.
    exit_auto: bool,

    /// Transient menu feedback line (import result, scan state).
    menu_note: Option<String>,
    /// Raw JSON of the last successfully imported ClientConfig.
    imported: Option<String>,

    // Pointer gesture state (drag-rotate / tap / wheel).
    /// Left button currently held (tracked from raw on_event).
    pointer_down: bool,
    /// Last raw pointer position (physical px) for drag deltas.
    last_phys: Option<(f64, f64)>,
    /// Accumulated travel during the current press — distinguishes drag vs tap.
    drag_moved: f64,
    /// Timestamp (ms) of the last centerpiece tap. Planet uses the gap to
    /// close an open node; connect itself does not wait on it.
    last_tap_ms: f64,

    /// Whether the pointer is currently inside the window. `pointer_pos()`
    /// keeps the last position after the cursor leaves, so without this the
    /// cursor-reactive skins freeze a warp spot at the exit point.
    pointer_in_window: bool,

    /// Which list field has the caret. Typing, paste, and a dropped file land here.
    list_editor: Option<ListEditor>,
    list_draft: String,
    /// Setup's first row. A claim code or `/c/` link.
    bind_focused: bool,
    bind_draft: String,
    /// Second press on ОТВЯЗАТЬ performs it. Any other press cancels.
    unlink_armed: bool,
    /// Debug lines. The sheet ticker shows the last one. A click opens the rest.
    log: Vec<String>,
    log_open: bool,
    born: Instant,
    file_hover: bool,
    sites_rect: Option<Rect>,
    apps_rect: Option<Rect>,
    key_mods: ModifierKeys,

    /// Backend link: one subscriber-client child, driven at 127.0.0.1:19421.
    engine: Box<dyn crate::backend::EngineLink>,

    /// Developer hooks (profiling sync, fps HUD) — `dev-tools` builds only.
    #[cfg(feature = "dev-tools")]
    dev: Option<crate::dev::DevHooks>,

    // Frame timing.
    last_frame: Instant,
}

impl MirageClientApp {
    /// Construct with an explicit initial skin (the `--skin` CLI flag) and the
    /// engine link that drives the connection state.
    pub fn new(skin_kind: SkinKind, engine: Box<dyn crate::backend::EngineLink>) -> Self {
        let unbound = !engine.is_bound();
        let bound_note = if unbound {
            Some("привязка: вставь код кабинета".to_owned())
        } else {
            None
        };
        Self {
            skin: make_skin(skin_kind),
            engine,
            skin_kind,
            current_phase: ConnPhase::Disconnected,
            pending_connect_at: None,
            menu_open: true,
            menu_tab: MenuTab::Setup,
            menu_frac: menu::DEFAULT_FRAC,
            menu_scroll: 0.0,
            country_scroll: 0.0,
            country_drag: false,
            country_grab: 0.0,
            country_dragged: false,
            country_view: None,
            last_pointer: None,
            menu_drag_sep: false,
            menu_last_y: 0.0,
            prev_pointer_down: false,
            selected_exit: None,
            countries: Vec::new(),
            exit_auto: true,
            menu_note: bound_note,
            imported: None,
            pointer_down: false,
            last_phys: None,
            drag_moved: 0.0,
            last_tap_ms: -1.0e9,
            pointer_in_window: true,
            list_editor: None,
            list_draft: String::new(),
            bind_focused: unbound,
            bind_draft: String::new(),
            unlink_armed: false,
            log: Vec::new(),
            log_open: false,
            born: Instant::now(),
            file_hover: false,
            sites_rect: None,
            apps_rect: None,
            key_mods: ModifierKeys::none(),
            #[cfg(feature = "dev-tools")]
            dev: None,
            last_frame: Instant::now(),
        }
    }

    /// Attach the developer hooks; honours their `--connected` start phase.
    #[cfg(feature = "dev-tools")]
    pub fn with_dev(mut self, dev: crate::dev::DevHooks) -> Self {
        if dev.start_connected() {
            self.current_phase = ConnPhase::Connected;
        }
        self.dev = Some(dev);
        self
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

    fn dismiss_list_editor(&mut self) {
        self.commit_list_draft();
        self.list_editor = None;
    }

    fn focus_side(&mut self, sites: bool) {
        let next = if sites { ListEditor::Sites } else { ListEditor::Apps };
        if self.list_editor == Some(next) {
            return;
        }
        self.commit_list_draft();
        self.bind_focused = false;
        self.list_editor = Some(next);
    }

    fn focus_bind(&mut self) {
        self.dismiss_list_editor();
        self.bind_focused = true;
    }

    fn remember(&mut self, text: &str) {
        if text.is_empty() || self.log.last().map(String::as_str) == Some(text) {
            return;
        }
        self.log.push(text.to_owned());
        if self.log.len() > 48 {
            let drop_n = self.log.len() - 48;
            self.log.drain(0..drop_n);
        }
    }

    fn submit_bind(&mut self) {
        let text = self.bind_draft.trim().to_owned();
        if text.is_empty() {
            self.menu_note = Some("поле пустое".to_owned());
            return;
        }
        self.menu_note = Some("привязка…".to_owned());
        self.engine.bind_claim(&text);
    }

    fn commit_list_draft(&mut self) {
        let Some(editor) = self.list_editor else {
            self.list_draft.clear();
            return;
        };
        let raw = std::mem::take(&mut self.list_draft);
        let extra = crate::config_import::names_from_text(&raw);
        if extra.is_empty() {
            return;
        }
        self.append_names(editor == ListEditor::Sites, &extra);
    }

    fn publish_scope(
        &mut self,
        services: &[String],
        apps: &[String],
        bypass_services: &[String],
        bypass_apps: &[String],
    ) {
        self.engine.set_scope(services, apps, bypass_services, bypass_apps);
        self.menu_note = None;
    }

    /// A typed or dropped name is a tunnel. The same name leaves the bypass list.
    fn append_names(&mut self, sites: bool, extra: &[String]) {
        let es = self.engine.status();
        let mut services = es.services.clone();
        let mut apps = es.apps.clone();
        let mut bypass_services = es.bypass_services.clone();
        let mut bypass_apps = es.bypass_apps.clone();
        let (dest, other) = if sites {
            (&mut services, &mut bypass_services)
        } else {
            (&mut apps, &mut bypass_apps)
        };
        for name in extra {
            other.retain(|have| have != name);
            if !dest.iter().any(|have| have == name) {
                dest.push(name.clone());
            }
        }
        self.publish_scope(&services, &apps, &bypass_services, &bypass_apps);
    }

    fn clear_side(&mut self, sites: bool) {
        if self.list_editor == Some(if sites { ListEditor::Sites } else { ListEditor::Apps }) {
            self.list_draft.clear();
        }
        let es = self.engine.status();
        let tun = es.mode == "tun";
        let mut services = es.services.clone();
        let mut apps = es.apps.clone();
        let mut bypass_services = es.bypass_services.clone();
        let mut bypass_apps = es.bypass_apps.clone();
        if sites {
            services.clear();
            if tun {
                bypass_services.clear();
            }
        } else {
            apps.clear();
            if tun {
                bypass_apps.clear();
            }
        }
        self.publish_scope(&services, &apps, &bypass_services, &bypass_apps);
    }

    fn remove_name(&mut self, sites: bool, index: usize) {
        let es = self.engine.status();
        let mut services = es.services.clone();
        let mut apps = es.apps.clone();
        let mut bypass_services = es.bypass_services.clone();
        let mut bypass_apps = es.bypass_apps.clone();
        let roles = es.mode == "tun";
        let (tunnel, bypass) = if sites {
            (&mut services, &mut bypass_services)
        } else {
            (&mut apps, &mut bypass_apps)
        };
        if roles && index >= tunnel.len() {
            let at = index - tunnel.len();
            if at < bypass.len() {
                bypass.remove(at);
            }
        } else if index < tunnel.len() {
            tunnel.remove(index);
        }
        self.publish_scope(&services, &apps, &bypass_services, &bypass_apps);
    }

    /// Move one TUN name between the tunnel list and the bypass list.
    fn flip_role(&mut self, sites: bool, index: usize) {
        let es = self.engine.status();
        if es.mode != "tun" {
            return;
        }
        let mut services = es.services.clone();
        let mut apps = es.apps.clone();
        let mut bypass_services = es.bypass_services.clone();
        let mut bypass_apps = es.bypass_apps.clone();
        let (tunnel, bypass) = if sites {
            (&mut services, &mut bypass_services)
        } else {
            (&mut apps, &mut bypass_apps)
        };
        if index < tunnel.len() {
            let name = tunnel.remove(index);
            if !bypass.iter().any(|have| have == &name) {
                bypass.push(name);
            }
        } else {
            let at = index - tunnel.len();
            if at < bypass.len() {
                let name = bypass.remove(at);
                if !tunnel.iter().any(|have| have == &name) {
                    tunnel.push(name);
                }
            }
        }
        self.publish_scope(&services, &apps, &bypass_services, &bypass_apps);
    }

    fn append_file(&mut self, sites: bool) {
        let Ok(names) = crate::config_import::list_from_file() else { return };
        if names.is_empty() {
            return;
        }
        self.append_names(sites, &names);
    }

    fn side_at(&self, x: f64, y: f64) -> Option<bool> {
        let hit = |rect: &Option<Rect>| {
            rect.as_ref().is_some_and(|r| x >= r.x && y >= r.y && x <= r.x + r.width && y < r.y + r.height)
        };
        if hit(&self.sites_rect) {
            Some(true)
        } else if hit(&self.apps_rect) {
            Some(false)
        } else {
            self.list_editor.map(|editor| editor == ListEditor::Sites)
        }
    }

    fn feed_list_text(&mut self, text: &str) {
        if self.list_editor.is_none() || !self.menu_open {
            return;
        }
        let mut commit = false;
        for ch in text.chars() {
            if ch == ',' || ch == ';' || ch == '\n' || ch == '\r' {
                commit = true;
            } else if !ch.is_control() {
                if commit {
                    self.commit_list_draft();
                    commit = false;
                }
                self.list_draft.push(ch);
            }
        }
        if commit {
            self.commit_list_draft();
        }
    }

    fn on_list_row(
        &mut self,
        sites: bool,
        w: f64,
        ry: f64,
        pointer: Option<(f64, f64)>,
    ) {
        let Some((px, py)) = pointer else { return };
        let es = self.engine.status();
        let roles = es.mode == "tun";
        let n = if sites {
            es.services.len() + if roles { es.bypass_services.len() } else { 0 }
        } else {
            es.apps.len() + if roles { es.bypass_apps.len() } else { 0 }
        };
        let Some(part) = menu::list_part_at(w, ry, n, px, py, roles) else { return };
        match part {
            ListPart::Clear => {
                if n > 0 {
                    self.clear_side(sites);
                } else {
                    self.focus_side(sites);
                }
            }
            ListPart::File => {
                self.focus_side(sites);
                self.commit_list_draft();
                self.append_file(sites);
            }
            ListPart::Field => self.focus_side(sites),
            ListPart::Chip(index) => self.remove_name(sites, index),
            ListPart::Role(index) => self.flip_role(sites, index),
        }
    }

    /// One tap connects, the next tap disconnects. No double-click.
    fn request_toggle(&mut self) {
        match self.current_phase {
            ConnPhase::Disconnected => self.request_connect(),
            ConnPhase::Connected | ConnPhase::Connecting => self.request_disconnect(),
            ConnPhase::Disconnecting => {}
        }
    }

    /// Single tap from a settled disconnected state.
    fn request_connect(&mut self) {
        if !self.engine.is_bound() {
            self.menu_open = true;
            self.menu_tab = MenuTab::Setup;
            self.focus_bind();
            self.menu_note = Some("привязка: вставь код кабинета".to_owned());
            return;
        }
        if matches!(self.current_phase, ConnPhase::Disconnected) {
            self.engine.connect();
            self.current_phase = ConnPhase::Connecting;
            self.pending_connect_at = Some(Instant::now());
        }
    }

    /// Apply a config-import result: stash the raw config + show a summary/error.
    fn import_config(&mut self, res: Result<crate::config_import::Imported, String>) {
        match res {
            Ok(imp) => {
                self.menu_note = Some(format!("импорт ок · {}", imp.summary));
                self.imported = Some(imp.raw);
            }
            Err(e) => {
                self.menu_note = Some(format!("импорт: {e}"));
            }
        }
    }

    /// Tap while up, or while still dialing, drops the session.
    fn request_disconnect(&mut self) {
        if matches!(self.current_phase, ConnPhase::Connected | ConnPhase::Connecting) {
            self.engine.disconnect();
            self.current_phase = ConnPhase::Disconnecting;
            self.pending_connect_at = None;
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
        if let Some(note) = self.engine.take_bind_note() {
            self.menu_note = Some(note);
        }

        // ── Phase: the selected carry must actually be up ───────────────────
        // `tun_alive` is the edge session. SOCKS needs its listener, HTTP
        // needs its listener, TUN needs the tun process.
        let es = self.engine.status();
        self.countries.clone_from(&es.countries);
        let carry = match es.mode.as_str() {
            "http" => es.http && es.tun_alive,
            "tun" => es.tun && es.tun_alive,
            _ => es.socks && es.tun_alive,
        };
        let tun_fault = es.mode == "tun" && es.error.is_some() && !es.tun;
        if carry {
            self.current_phase = ConnPhase::Connected;
            self.pending_connect_at = None;
        } else {
            match self.current_phase {
                // Hold "Connecting" only while the dial has no fault yet.
                // A tun fault drops it immediately so the globe does not fold.
                ConnPhase::Connecting => {
                    let gave_up = self.pending_connect_at.map_or(true, |at| at.elapsed().as_millis() > 40_000);
                    if tun_fault || gave_up {
                        self.current_phase = ConnPhase::Disconnected;
                        self.pending_connect_at = None;
                    }
                }
                ConnPhase::Disconnecting => {
                    self.current_phase = ConnPhase::Disconnected;
                }
                // tun went down underneath us.
                ConnPhase::Connected => self.current_phase = ConnPhase::Disconnected,
                ConnPhase::Disconnected => {}
            }
        }

        let w = win.rect.width;
        let h = win.rect.height;
        // None when the cursor is outside the window so cursor-reactive skins
        // heal instead of freezing the warp at the last position.
        let pointer = if self.pointer_in_window {
            win.layout.pointer_pos()
        } else {
            None
        };

        // Dev-tools: profiling sync (+ requested backend hot-switch) and the
        // HUD readout.
        #[cfg(feature = "dev-tools")]
        let hud = self.dev.as_ref().map(|d| d.frame(&mut *win.render_control));

        // ── Menu container (resizable / scrollable / tabbed) ──────────────────
        // Compute geometry + active-tab rows once per frame while open.
        let menu = if self.menu_open {
            let tabs: &[MenuTab] = &MenuTab::ALL;
            let area = (h - CHROME_H).max(1.0);
            let panel_h = (area * self.menu_frac)
                .clamp(area * menu::MIN_FRAC, area * menu::MAX_FRAC);
            let g = menu::geom(w, h, panel_h, tabs.len());
            let rows = menu::tab_rows(self.menu_tab, self.exit_auto, self.countries.len());
            let roles = es.mode == "tun";
            let site_n = es.services.len() + if roles { es.bypass_services.len() } else { 0 };
            let app_n = es.apps.len() + if roles { es.bypass_apps.len() } else { 0 };
            let (laid, content_h) = menu::layout_rows(
                &rows,
                self.countries.len(),
                site_n,
                app_n,
            );
            Some((g, tabs, laid, content_h))
        } else {
            None
        };

        // ── Process layout clicks / resize / scroll BEFORE painting ────────────
        if let Some((g, tabs, laid, content_h)) = &menu {
            let area = (h - CHROME_H).max(1.0);

            // Bottom separator drag → resize the panel.
            let over_sep = pointer
                .map(|(_, py)| py >= g.sep.y - 3.0 && py <= g.sep.y + g.sep.height + 3.0)
                .unwrap_or(false);
            if self.pointer_down && !self.prev_pointer_down && over_sep {
                self.menu_drag_sep = true;
                self.menu_last_y = pointer.map(|(_, py)| py).unwrap_or(0.0);
            }
            if self.menu_drag_sep && self.pointer_down {
                if let Some((_, py)) = pointer {
                    self.menu_frac =
                        (self.menu_frac + (py - self.menu_last_y) / area).clamp(menu::MIN_FRAC, menu::MAX_FRAC);
                    self.menu_last_y = py;
                }
            }
            if !self.pointer_down {
                self.menu_drag_sep = false;
            }

            // Clamp scroll to content.
            let max_scroll = (content_h - g.content.height).max(0.0);
            self.menu_scroll = self.menu_scroll.clamp(0.0, max_scroll);

            let country_row_y = laid.iter().find(|lr| matches!(lr.row, MenuRow::Countries)).map(|lr| {
                g.content.y - self.menu_scroll + lr.y
            });
            self.country_view = country_row_y.and_then(|row_y| {
                menu::country_scroll_geom(w, row_y, self.countries.len(), self.country_scroll)
            });
            if let Some(max_c) = self.country_view.as_ref().map(|view| view.max_scroll) {
                self.country_scroll = self.country_scroll.clamp(0.0, max_c);
            }
            let thumb = self.country_view.as_ref().and_then(|view| view.thumb);
            if self.pointer_down && !self.prev_pointer_down {
                if let (Some(thumb), Some((px, py))) = (thumb, pointer) {
                    let on_thumb = px >= thumb.x - 3.0
                        && px <= thumb.x + thumb.width + 3.0
                        && py >= thumb.y
                        && py <= thumb.y + thumb.height;
                    if on_thumb {
                        self.country_drag = true;
                        self.country_grab = py - thumb.y;
                        self.country_dragged = false;
                    }
                }
            }
            let track = self.country_view.as_ref().and_then(|view| view.track);
            let thumb_h = self.country_view.as_ref().and_then(|view| view.thumb).map(|t| t.height).unwrap_or(12.0);
            let max_c = self.country_view.as_ref().map(|view| view.max_scroll).unwrap_or(0.0);
            if self.country_drag && self.pointer_down {
                if let (Some(track), Some((_, py))) = (track, pointer) {
                    let travel = (track.height - thumb_h).max(1.0);
                    let t = ((py - self.country_grab - track.y) / travel).clamp(0.0, 1.0);
                    let next = t * max_c;
                    if (next - self.country_scroll).abs() > 0.5 {
                        self.country_dragged = true;
                    }
                    self.country_scroll = next;
                }
            }

            // Scrim → close.
            if win.layout.was_clicked(&id_scrim()) {
                self.dismiss_list_editor();
                self.bind_focused = false;
                self.unlink_armed = false;
                self.log_open = false;
                self.menu_open = false;
            }
            let on_ticker = win.layout.was_clicked(&id_ticker());
            let on_log = win.layout.was_clicked(&id_log());
            if on_ticker {
                self.log_open = !self.log_open;
            }
            // Tabs.
            for (i, t) in tabs.iter().enumerate() {
                if win.layout.was_clicked(&id_menu_tab(i)) {
                    self.menu_tab = *t;
                    self.menu_scroll = 0.0;
                }
            }
            // Visible content rows (same cull as draw/registration).
            let cy = g.content.y - self.menu_scroll;
            for (i, lr) in laid.iter().enumerate() {
                if on_ticker || on_log || !lr.clickable {
                    continue;
                }
                let ry = cy + lr.y;
                let visible = ry + lr.h > g.content.y && ry < g.content.y + g.content.height;
                if !visible || !win.layout.was_clicked(&id_menu_row(i)) {
                    continue;
                }
                if !matches!(lr.row, MenuRow::Bind) {
                    self.unlink_armed = false;
                }
                self.log_open = false;
                match lr.row {
                    MenuRow::ExitMode => {
                        // Click within the right-aligned pill → pick by half;
                        // a click on the label just toggles.
                        let (px, pill_w) = menu::exit_pill_box(w);
                        let half = pill_w / 2.0;
                        self.exit_auto = match pointer {
                            Some((cxp, _)) if cxp >= px => cxp < px + half,
                            _ => !self.exit_auto,
                        };
                        if self.exit_auto {
                            self.country_scroll = 0.0;
                            self.engine.set_exit("auto");
                        } else if let Some(code) = self.selected_exit.clone() {
                            self.engine.set_exit(&code);
                        }
                    }
                    MenuRow::Countries => {
                        if self.country_dragged {
                            // Thumb drag, not a country pick.
                        } else if let Some((px, py)) = pointer {
                            if let Some(n) = menu::country_index_at(
                                w,
                                ry,
                                self.countries.len(),
                                self.country_scroll,
                                px,
                                py,
                            ) {
                                if let Some(code) = self.countries.get(n).cloned() {
                                    self.selected_exit = Some(code.clone());
                                    self.exit_auto = false;
                                    self.engine.set_exit(&code);
                                }
                            }
                        }
                    }
                    MenuRow::Skin(s) => {
                        self.dismiss_list_editor();
                        self.bind_focused = false;
                        self.switch_skin(s);
                        self.menu_open = false;
                        self.menu_note = None;
                    }
                    MenuRow::Bind => {
                        if self.engine.is_bound() {
                            let px = pointer.map(|(px, _)| px);
                            let unlink_x = menu::unlink_button_x(w);
                            let on_unlink = px.is_some_and(|x| x >= unlink_x);
                            let on_cabinet = self.engine.cabinet_open()
                                && px.is_some_and(|x| x >= menu::cabinet_button_x(w) && x < unlink_x);
                            if on_unlink && self.unlink_armed {
                                self.unlink_armed = false;
                                self.engine.unlink();
                                self.bind_draft.clear();
                                self.bind_focused = false;
                                self.menu_note = Some("отвязано".to_owned());
                            } else if on_unlink {
                                self.unlink_armed = true;
                                self.menu_note = Some("ещё раз — отвязать".to_owned());
                            } else if on_cabinet {
                                self.unlink_armed = false;
                                open_cabinet();
                                self.menu_note = Some("кабинет: браузер".to_owned());
                            }
                        } else {
                            let submit = pointer.is_some_and(|(px, _)| px >= menu::bind_button_x(w));
                            if submit {
                                self.submit_bind();
                            } else {
                                self.focus_bind();
                            }
                        }
                    }
                    MenuRow::Action(MenuAction::ImportFile) => {
                        self.import_config(crate::config_import::from_file());
                    }
                    MenuRow::Action(MenuAction::ImportText) => {
                        self.import_config(crate::config_import::from_clipboard_text());
                    }
                    MenuRow::Action(MenuAction::ImportQrClip) => {
                        self.import_config(crate::config_import::from_clipboard_qr());
                    }
                    MenuRow::ToggleSocks => {
                        if es.mode == "http" || es.mode == "tun" {
                            self.engine.set_listeners(true, false);
                        }
                    }
                    MenuRow::ToggleHttp => {
                        if es.mode != "http" {
                            self.engine.set_listeners(false, true);
                        }
                    }
                    MenuRow::ToggleTun => {
                        if es.mode != "tun" {
                            self.engine.set_tun(true);
                        }
                    }
                    MenuRow::Sites => self.on_list_row(true, w, ry, pointer),
                    MenuRow::Apps => self.on_list_row(false, w, ry, pointer),
                }
            }
            if !self.pointer_down {
                self.country_drag = false;
                self.country_dragged = false;
            }
        } else {
            self.country_view = None;
        }
        self.last_pointer = pointer;
        self.prev_pointer_down = self.pointer_down;

        if win.layout.was_clicked(&id_restart()) {
            if matches!(mirage_client_gui::ota::status(), mirage_client_gui::ota::UpdateStatus::RestartPending { .. }) {
                mirage_client_gui::ota::restart();
            }
        }

        if !self.menu_open {
            // Tap on the centerpiece (menu closed). Drag-rotate and wheel-zoom
            // are handled in `on_event`. A tap barely moved (`drag_moved < TAP_SLOP`).
            // Connect toggles on that tap. The only double-tap left is Planet
            // closing an open node — a single click there rotates the globe.
            let now_ms = win.layout.frame_time_ms();
            if win.layout.was_clicked(&id_centerpiece()) && self.drag_moved < TAP_SLOP {
                let (tx, ty) = win.layout.pointer_pos().unwrap_or((0.0, 0.0));
                let double = now_ms - self.last_tap_ms < DBLCLICK_MS;
                self.last_tap_ms = now_ms;
                if double && self.skin.double_tap(tx, ty) {
                    // Planet closed its node. Do not also toggle the session.
                } else if !self.skin.tap(tx, ty) {
                    self.request_toggle();
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

        // ── Menu overlay (when open) ──────────────────────────────────────────
        if let Some((g, tabs, laid, content_h)) = &menu {
            let fault = if es.mode == "tun" { es.error.clone() } else { None };
            let shown = match self.menu_note.as_deref() {
                Some(text)
                    if text.starts_with("привяз")
                        || text.starts_with("кабинет")
                        || text == "поле пустое"
                        || text.starts_with("ещё раз")
                        || text == "отвязано" =>
                {
                    Some(text.to_owned())
                }
                _ => fault.or_else(|| self.menu_note.clone()),
            };
            if let Some(text) = &shown {
                self.remember(text);
            }
            let note = self.log.last().map(String::as_str);
            menu::draw_menu(
                render,
                w,
                h,
                g,
                self.menu_tab,
                tabs,
                laid,
                *content_h,
                self.menu_scroll,
                self.country_scroll,
                self.skin_kind,
                &self.countries,
                self.selected_exit.as_deref(),
                self.exit_auto,
                note,
                es.mode.as_str(),
                es.socks,
                es.http,
                es.tun,
                &es.services,
                &es.apps,
                &es.bypass_services,
                &es.bypass_apps,
                self.list_editor == Some(ListEditor::Sites),
                self.list_editor == Some(ListEditor::Apps),
                &self.list_draft,
                self.bind_focused,
                &self.bind_draft,
                self.engine.is_bound(),
                self.unlink_armed,
                self.engine.cabinet_open(),
                self.engine.bound_name().as_deref(),
                self.born.elapsed().as_secs_f64(),
                &self.log,
                self.log_open,
                self.file_hover,
                pointer,
            );
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

            if let Some((g, tabs, laid, _)) = &menu {
                // ── Menu open: scrim → panel → tabs → visible rows ────────────
                // Hit priority follows registration order (later wins).
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
                    height: g.panel.height,
                });
                for i in 0..tabs.len() {
                    let r = g.tabs[i];
                    register_hit(layout, id_menu_tab(i), Rect { x: r.x, y: r.y, width: r.width, height: r.height });
                }
                let cy = g.content.y - self.menu_scroll;
                self.sites_rect = None;
                self.apps_rect = None;
                for (i, lr) in laid.iter().enumerate() {
                    if !lr.clickable {
                        continue;
                    }
                    let ry = cy + lr.y;
                    if ry + lr.h > g.content.y && ry < g.content.y + g.content.height {
                        let rect = Rect { x: 0.0, y: ry, width: w, height: lr.h };
                        if matches!(lr.row, MenuRow::Sites) {
                            self.sites_rect = Some(rect);
                        } else if matches!(lr.row, MenuRow::Apps) {
                            self.apps_rect = Some(rect);
                        }
                        register_hit(layout, id_menu_row(i), rect);
                    }
                }
                if !self.log.is_empty() {
                    let ty = g.panel.y + g.panel.height - 26.0;
                    register_hit(layout, id_ticker(), Rect { x: 0.0, y: ty, width: w, height: 22.0 });
                    if self.log_open {
                        let rows = self.log.len().min(8) as f64;
                        let ph = rows * 16.0 + 8.0;
                        register_hit(layout, id_log(), Rect {
                            x: 8.0,
                            y: (ty - ph - 2.0).max(CHROME_H),
                            width: (w - 16.0).max(0.0),
                            height: ph,
                        });
                    }
                }
            } else {
                self.sites_rect = None;
                self.apps_rect = None;
                // ── Menu closed: invisible centerpiece hit-zone ───────────────
                register_hit(layout, id_centerpiece(), Rect {
                    x: 0.0,
                    y: CHROME_H,
                    width: w,
                    height: (h - CHROME_H).max(0.0),
                });
            }
            if matches!(
                mirage_client_gui::ota::status(),
                mirage_client_gui::ota::UpdateStatus::Downloading { .. }
                    | mirage_client_gui::ota::UpdateStatus::Verifying
                    | mirage_client_gui::ota::UpdateStatus::Installing
                    | mirage_client_gui::ota::UpdateStatus::RestartPending { .. }
            ) {
                register_hit(layout, id_restart(), restart_rect(w));
            }
        }

        // ── Plain "M" menu glyph + kill the chrome's bottom separator line ────
        // Painted last, over the chrome's default hamburger.
        {
            let render = &mut *win.render;
            const SLOT_W: f64 = 36.0;
            render.set_fill_color("#0C0C10");
            render.fill_rect(0.0, 0.0, SLOT_W, CHROME_H);
            // Cover the chrome bottom border (the unwanted under-chrome line).
            render.fill_rect(0.0, CHROME_H - 1.0, w, 1.0);

            render.set_fill_color("#FBB26A");
            render.set_font("18px sans-serif");
            render.set_text_align(TextAlign::Center);
            render.set_text_baseline(TextBaseline::Middle);
            render.fill_text("M", SLOT_W / 2.0, CHROME_H / 2.0 - 1.0);
        }

        // ── fps HUD (dev-tools; top-right, under chrome; hidden when menu open)
        #[cfg(feature = "dev-tools")]
        if let (Some(text), false) = (&hud, self.menu_open) {
            crate::dev::draw_hud(&mut *win.render, w, CHROME_H, text);
        }

        // Painted last so an installed update stays on top of the open menu.
        // The accent reading is the restart control. A download only reports
        // progress; it does not ask for a click.
        let update = mirage_client_gui::ota::status();
        let banner: Option<(String, bool)> = match &update {
            mirage_client_gui::ota::UpdateStatus::Downloading { percent } => Some((format!("загрузка {percent}%"), false)),
            mirage_client_gui::ota::UpdateStatus::Verifying | mirage_client_gui::ota::UpdateStatus::Installing => {
                Some(("установка".to_owned(), false))
            }
            mirage_client_gui::ota::UpdateStatus::RestartPending { .. } => Some(("Перезагрузить".to_owned(), true)),
            _ => None,
        };
        if let Some((label, ready)) = banner {
            let rect = restart_rect(w);
            let render = &mut *win.render;
            render.set_fill_color(if ready { "#FBB26A" } else { "#1A1A22" });
            render.fill_rect(rect.x, rect.y, rect.width, rect.height);
            render.set_fill_color(if ready { "#0C0C10" } else { "#F4F4F5" });
            render.set_font("14px sans-serif");
            render.set_text_align(TextAlign::Center);
            render.set_text_baseline(TextBaseline::Middle);
            render.fill_text(&label, rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
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
                if self.menu_open {
                    self.dismiss_list_editor();
                    self.bind_focused = false;
                }
                self.menu_open = !self.menu_open;
                if !self.menu_open {
                    self.menu_note = None;
                }
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
                if self.menu_open && self.bind_focused {
                    match key {
                        KeyCode::Backspace => {
                            self.bind_draft.pop();
                            return true;
                        }
                        KeyCode::Enter => {
                            self.submit_bind();
                            return true;
                        }
                        KeyCode::Escape => {
                            self.bind_focused = false;
                            return true;
                        }
                        KeyCode::V if self.key_mods.ctrl || self.key_mods.meta => {
                            if let Some(text) = crate::config_import::clipboard_text() {
                                for ch in text.chars() {
                                    if !ch.is_control() {
                                        self.bind_draft.push(ch);
                                    }
                                }
                            }
                            return true;
                        }
                        _ => return true,
                    }
                }
                if self.menu_open && self.list_editor.is_some() {
                    match key {
                        KeyCode::Backspace => {
                            self.list_draft.pop();
                            return true;
                        }
                        KeyCode::Enter => {
                            self.commit_list_draft();
                            return true;
                        }
                        KeyCode::Escape => {
                            self.list_draft.clear();
                            self.list_editor = None;
                            return true;
                        }
                        KeyCode::V if self.key_mods.ctrl || self.key_mods.meta => {
                            if let Some(text) = crate::config_import::clipboard_text() {
                                self.feed_list_text(&text);
                            }
                            return true;
                        }
                        _ => return true,
                    }
                }
                match key {
                    KeyCode::Num1 => { self.switch_skin(SkinKind::Bloom); true }
                    KeyCode::Num2 => { self.switch_skin(SkinKind::Planet); true }
                    KeyCode::Tab  => { self.cycle_skin(); true }
                    _             => false,
                }
            }
            PlatformEvent::TextInput { text } => {
                if self.key_mods.ctrl || self.key_mods.meta {
                    return false;
                }
                if self.menu_open && self.bind_focused {
                    for ch in text.chars() {
                        if !ch.is_control() {
                            self.bind_draft.push(ch);
                        }
                    }
                    return true;
                }
                self.feed_list_text(text);
                self.list_editor.is_some()
            }
            PlatformEvent::Ime(ImeEvent::Commit(text)) => {
                if self.menu_open && self.bind_focused {
                    for ch in text.chars() {
                        if !ch.is_control() {
                            self.bind_draft.push(ch);
                        }
                    }
                    return true;
                }
                self.feed_list_text(text);
                self.list_editor.is_some()
            }
            PlatformEvent::ModifiersChanged { modifiers } => {
                self.key_mods = *modifiers;
                false
            }
            PlatformEvent::FileHovered { .. } => {
                self.file_hover = self.menu_open;
                self.menu_open
            }
            PlatformEvent::FileCancelled => {
                self.file_hover = false;
                false
            }
            PlatformEvent::FileDropped { path } => {
                self.file_hover = false;
                if !self.menu_open {
                    return false;
                }
                let Some((x, y)) = self.last_pointer else { return false };
                let Some(sites) = self.side_at(x, y) else { return false };
                match crate::config_import::names_from_path(path) {
                    Ok(names) if !names.is_empty() => {
                        self.append_names(sites, &names);
                    }
                    Ok(_) => {}
                    Err(e) => self.menu_note = Some(e),
                }
                true
            }
            // Drag-rotate: feed raw pointer deltas to the active skin while the
            // button is held (planet rotates; bloom ignores drag). Coordinates
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
                        let travel = (x - lx).abs() + (y - ly).abs();
                        let next = self.drag_moved + travel;
                        // A click that barely moves must not spin the planet.
                        if next >= TAP_SLOP {
                            self.skin.drag(x - lx, y - ly);
                        }
                        self.drag_moved = next;
                    }
                }
                self.last_phys = Some((*x, *y));
                false
            }
            // Wheel → scroll the menu when open, else zoom the skin (planet).
            PlatformEvent::Scroll { dy, .. } => {
                if self.menu_open {
                    let over_grid = match (&self.country_view, self.last_pointer) {
                        (Some(view), Some((px, py))) => {
                            view.max_scroll > 0.0
                                && px >= view.viewport.x
                                && py >= view.viewport.y
                                && px <= view.viewport.x + view.viewport.width
                                && py <= view.viewport.y + view.viewport.height
                        }
                        _ => false,
                    };
                    if over_grid {
                        let max = self.country_view.as_ref().map(|v| v.max_scroll).unwrap_or(0.0);
                        self.country_scroll = (self.country_scroll - *dy).clamp(0.0, max);
                    } else {
                        self.menu_scroll = (self.menu_scroll - *dy).max(0.0);
                    }
                } else {
                    self.skin.zoom(*dy);
                }
                false
            }
            // Track cursor entering/leaving the window so reactive skins heal.
            PlatformEvent::PointerEntered => {
                self.pointer_in_window = true;
                false
            }
            PlatformEvent::PointerLeft => {
                self.pointer_in_window = false;
                false
            }
            _ => false,
        }
    }
}
