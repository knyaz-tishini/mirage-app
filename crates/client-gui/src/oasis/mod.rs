//! m1r4g3 desktop shell. The oasis scene fills the window. Chrome is a
//! transparent overlay: a stroked amber palm (no fill) on the left, and
//! minimize / close on the right drawn like a system caption. The empty
//! band drags the window while the sheet is closed. An open sheet fills
//! from the top, including the chrome band, and does not dim the scene.
//! Cabinet and standalone sit on that fill, level with the palm and the
//! caption buttons, and only while the sheet is open. Text rows are
//! framework text fields. A click on the
//! scene eases the link over 0.55s.

use std::collections::HashMap;

use uzor::input::keyboard::KeyCode;
use uzor::input::{Sense, WidgetKind};
use uzor::layout::docking::DockPanel;
use uzor::render::{AnimParams, AnimScript, RenderContext, TextAlign, TextBaseline};
use uzor::tokens::{BuiltinSet, Tokens};
use uzor::{Rect, RgbaIcon, TextFieldConfig, WidgetId};
use uzor_anim_js::JsAnimHost;
use uzor_framework::host::native::{NativeOptions, RenderPreference, TraySpec, run_native};
use uzor_framework::{
    App, AppCommand, ChromeModel, ControlsPlacement, DockTarget, FloatingControls, Intent, IntentCx,
    KeyInput, KeyState, LayoutCmd, OverlayCx, OverlayModel, PanelCx, PanelHome, RuntimeConfig, SizePx,
    Spec, TextIntent, UnhandledInput, WindowCmd, WindowControlsStyle, WindowId, WindowIntent, WindowSpec,
};

use crate::backend::{EngineLink, LocalHttp};
use crate::shell::menu::{self, MenuTab};

mod provision;
mod stand;

use stand::Stand;

const HIT_SCENE: &str = "scene";
const TRANSITION_S: f64 = 0.55;
const SCALE: f64 = 1.75 * 0.75;
const CHROME_H: f64 = 32.0 * SCALE;
const MARK: f64 = 28.0 * SCALE;
const GLYPH: f64 = 22.0 * SCALE;
const LIP: f64 = 1.0;
const PALM_W: u32 = 256;
const PALM: &[u8] = include_bytes!("palm256.rgba");
const PALM_TRAY_W: u32 = 32;
const PALM_TRAY: &[u8] = include_bytes!("palm32.rgba");

#[derive(Clone, Copy, Debug)]
struct Scene;

impl DockPanel for Scene {
    fn title(&self) -> &str {
        "m1r4g3"
    }

    fn type_id(&self) -> &'static str {
        "oasis"
    }

    fn shows_header(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Ov {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Act {}

struct Vocab;

impl Spec for Vocab {
    type Panel = Scene;
    type Overlay = Ov;
    type Action = Act;

    fn decode_panel(_home: PanelHome, _type_id: &str) -> Option<Scene> {
        Some(Scene)
    }
}

#[derive(Clone)]
struct WinUi {
    menu: bool,
    scroll: f64,
}

impl Default for WinUi {
    fn default() -> Self {
        Self { menu: false, scroll: 0.0 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Product {
    Cabinet,
    Standalone,
}

struct Oasis {
    host: JsAnimHost,
    link: f64,
    target: f64,
    style: f64,
    toggles: u32,
    scratch: Vec<f64>,
    paint_err: bool,
    eased_at: f64,
    ui: HashMap<WindowId, WinUi>,
    product: Product,
    engine: LocalHttp,
    bind_draft: String,
    bind_focused: bool,
    unlink_armed: bool,
    note: String,
    fell_back: bool,
    stand: Stand,
    /// Scene button has started the configured connection. The boot picture is not this.
    linked: bool,
    /// Last standalone link handed to the subscriber. Stops a re-spawn every frame.
    adopted: String,
    pending_submit: Option<String>,
    /// Bumped when BIND clears the draft. Each window wipes its own field store once.
    bind_reset_epoch: u64,
    bind_reset_seen: HashMap<WindowId, u64>,
    /// Windows that already applied [`Stand::pass_epoch`].
    pass_reset_seen: HashMap<WindowId, u64>,
    /// Windows that already applied [`Stand::clear_epoch`].
    clear_seen: HashMap<WindowId, u64>,
    /// Windows that already pushed [`Stand::link_epoch`] into the link field.
    link_force_seen: HashMap<WindowId, u64>,
    scan: Option<crate::camera::CameraScan>,
}

impl Oasis {
    fn ease(&mut self, now: f64, dt: f64) {
        if (now - self.eased_at).abs() < 1e-6 {
            return;
        }
        self.eased_at = now;
        let step = dt / TRANSITION_S;
        if self.link < self.target {
            self.link = (self.link + step).min(self.target);
        } else if self.link > self.target {
            self.link = (self.link - step).max(self.target);
        }
    }
}

impl App for Oasis {
    type Spec = Vocab;

    fn overlay_model(&self, _win: WindowId, id: Ov) -> OverlayModel {
        match id {}
    }

    fn chrome_model(&self, _win: WindowId) -> ChromeModel {
        ChromeModel {
            floating: Some(FloatingControls {
                placement: ControlsPlacement::Right,
                style: WindowControlsStyle {
                    fill: None,
                    button_w: 0.0,
                    button_h: 0.0,
                    glyph_size: 0.0,
                    stroke: 0.0,
                    ..WindowControlsStyle::default()
                },
                drag_height: CHROME_H,
                show_maximize: false,
                show_minimize: false,
                border: None,
                radius: 0.0,
            }),
            ..ChromeModel::default()
        }
    }

    fn panel(&mut self, cx: &mut PanelCx<'_, Vocab>) {
        let rect = cx.rect;
        let win = cx.window;
        let now = cx.time.now.0;
        let dt = cx.time.dt;
        let w = rect.width;
        let h = rect.height;
        let scene_h = (h - CHROME_H).max(1.0);
        let menu_r = menu_rect();
        let min_r = glyph_rect(w, 1);
        let close_r = glyph_rect(w, 0);
        let id_menu = format!("chrome-m-{}", win.0);
        let id_min = format!("chrome-min-{}", win.0);
        let id_close = format!("chrome-x-{}", win.0);
        self.ui.entry(win).or_default();

        let layer = cx.widgets.layer.clone();
        let hit = |coord: &mut uzor::input::InputCoordinator, id: &str, r: Rect| {
            coord.register_atomic(id, WidgetKind::Button, r, Sense::CLICK, &layer);
        };
        let at = |local: Rect| Rect::new(rect.x + local.x, rect.y + local.y, local.width, local.height);

        if self.target >= 1.0 && !self.fell_back && self.engine.status().tun_block.is_some() {
            self.engine.set_listeners(true, false);
            self.fell_back = true;
        }
        if let Some(line) = self.engine.take_bind_note() {
            self.note = line;
        }
        self.stand.pull_bus();
        self.poll_scan();
        if !self.take_entrance() {
            self.sync_standalone_link();
        }

        let mut ui = self.ui.get(&win).cloned().unwrap_or_default();
        let panel_h = self.sheet_panel_h(h);
        let g = open_geom(w, h, panel_h);
        let (mode_cab, mode_std) = mode_pair(w);

        if ui.menu {
            // Hover only: blocks window-drag on the sheet, and does not take
            // the click. Text fields and buttons register after this.
            cx.widgets.coord.register_atomic(
                "sheet",
                WidgetKind::Custom,
                at(g.panel),
                Sense::HOVER,
                &layer,
            );
            let bottom = g.panel.y + g.panel.height;
            if bottom < h {
                hit(cx.widgets.coord, "scrim", at(Rect::new(0.0, bottom, w, (h - bottom).max(0.0))));
            }
            if self.product == Product::Cabinet {
                self.register_bind(cx.widgets.coord, &cx.view, &layer, &at, &g, w);
            } else {
                stand::register(cx.widgets.coord, &cx.view, &layer, &at, &g, &mut self.stand);
                if self.stand.take_scan_request() {
                    self.scan = Some(crate::camera::CameraScan::start());
                }
                if self.stand.take_scope() {
                    self.apply_scope();
                }
                self.take_entrance();
            }
            if cx.view.clicked(&WidgetId::from("scrim")) {
                ui.menu = false;
                self.bind_focused = false;
            }
            hit(cx.widgets.coord, "mode-cab", at(mode_cab));
            hit(cx.widgets.coord, "mode-std", at(mode_std));
            if cx.view.clicked(&WidgetId::from("mode-cab")) {
                self.set_mode(Product::Cabinet);
            }
            if cx.view.clicked(&WidgetId::from("mode-std")) {
                self.set_mode(Product::Standalone);
            }
        } else {
            hit(cx.widgets.coord, HIT_SCENE, at(Rect::new(0.0, CHROME_H, w, scene_h)));
        }
        hit(cx.widgets.coord, &id_menu, at(menu_r));
        hit(cx.widgets.coord, &id_min, at(min_r));
        hit(cx.widgets.coord, &id_close, at(close_r));
        let mut scan_cancel = false;
        if self.scan.is_some() {
            hit(
                cx.widgets.coord,
                "scan-cancel",
                at(Rect::new(0.0, CHROME_H, w, (h - CHROME_H).max(0.0))),
            );
            if cx.view.clicked(&WidgetId::from("scan-cancel")) {
                scan_cancel = true;
                self.scan = None;
            }
        }
        let menu_click = cx.view.clicked(&WidgetId::from(id_menu.as_str()));
        let min_click = cx.view.clicked(&WidgetId::from(id_min.as_str()));
        let close_click = cx.view.clicked(&WidgetId::from(id_close.as_str()));
        if !scan_cancel
            && self.scan.is_none()
            && !ui.menu
            && cx.view.clicked(&WidgetId::from(HIT_SCENE))
            && !menu_click
            && !min_click
            && !close_click
        {
            if self.linked {
                self.linked = false;
                self.target = 0.0;
                self.style = (self.toggles % 4) as f64;
                self.toggles = self.toggles.wrapping_add(1);
                self.stop_link();
            } else if self.connection_ready() && self.raise_link() {
                self.linked = true;
                self.target = 1.0;
                self.style = (self.toggles % 4) as f64;
                self.toggles = self.toggles.wrapping_add(1);
            }
        }
        if menu_click {
            ui.menu = !ui.menu;
        }
        if min_click {
            cx.out.command(AppCommand::Window(WindowCmd::Minimize { win, minimized: true }));
        }
        if close_click {
            cx.out.command(AppCommand::Window(WindowCmd::Close(win)));
        }
        self.ui.insert(win, ui);

        self.ease(now, dt);
        let (hour, day, dst) = local_clock();
        let mouse = cx.view.pointer_over(&WidgetId::from(HIT_SCENE)).map(|(x, y)| {
            (x - rect.x, y - rect.y)
        });
        let params = AnimParams {
            w,
            h,
            seed: 1,
            mouse,
            args: [self.link, hour, day, dst, self.style, self.target, 0.0, 0.0],
        };
        let menu_now = self.ui.get(&win).cloned().unwrap_or_default();
        let panel_h = self.sheet_panel_h(h);
        let pointer = cx.view.raw_pointer().map(|(x, y)| (x - rect.x, y - rect.y));
        let bind_hot = cx.view.is_focused(&WidgetId::from("bind"));
        cx.out.request_frame();
        let render: &mut dyn RenderContext = cx.render;
        render.save();
        render.translate(rect.x, rect.y);
        render.set_fill_color("#0C0C10");
        render.fill_rect(0.0, 0.0, w, h);
        render.save();
        let mut anim = self.host.anim("desert");
        if let Err(err) = anim.paint(render, now, &params, &mut self.scratch) {
            if !self.paint_err {
                eprintln!("oasis paint: {err}");
                self.paint_err = true;
            }
        }
        render.restore();
        if menu_now.menu {
            let g = open_geom(w, h, panel_h);
            if self.product == Product::Cabinet {
                let rows = menu::cabinet_rows();
                let (laid, content_h) = menu::layout_rows(&rows, 0, 0, 0);
                let note = if self.note.is_empty() { None } else { Some(self.note.as_str()) };
                menu::draw_menu(
                    render,
                    w,
                    h,
                    &g,
                    MenuTab::Setup,
                    &[],
                    &laid,
                    content_h,
                    menu_now.scroll,
                    0.0,
                    &[],
                    None,
                    true,
                    note,
                    "",
                    false,
                    false,
                    false,
                    &[],
                    &[],
                    &[],
                    &[],
                    false,
                    false,
                    "",
                    false,
                    "",
                    self.engine.is_bound(),
                    self.unlink_armed,
                    self.engine.cabinet_open(),
                    self.engine.bound_name().as_deref(),
                    now,
                    &[],
                    false,
                    false,
                    pointer,
                );
                if !self.engine.is_bound() {
                    if let Some(field) = bind_field_local(&g, w) {
                        render.set_fill_color(if bind_hot {
                            "rgba(251,178,106,0.10)"
                        } else {
                            "rgba(255,255,255,0.05)"
                        });
                        render.fill_rect(field.x, field.y, field.width, field.height);
                        if bind_hot {
                            render.set_fill_color("#FBB26A");
                            render.fill_rect(field.x, field.y + field.height - 2.0, field.width, 2.0);
                        }
                    }
                }
            } else {
                let note = if self.note.is_empty() { None } else { Some(self.note.as_str()) };
                stand::paint(render, &g, &self.stand, false, note);
            }
        }
        if menu_now.menu {
            let (mode_cab, mode_std) = mode_pair(w);
            paint_modes(render, mode_cab, mode_std, self.product);
        }
        if let Err(err) = paint_menu(render, &self.host, &mut self.scratch, now) {
            if !self.paint_err {
                eprintln!("menu paint: {err}");
                self.paint_err = true;
            }
        }
        paint_controls(render, w, pointer);
        if let Some(scan) = self.scan.as_ref() {
            crate::camera::draw_overlay(render, w, h, CHROME_H, scan);
        }
        render.restore();
        if menu_now.menu {
            if let Some(id) = self.pending_submit.take() {
                self.on_submit(&id);
            }
            self.take_entrance();
            let g = open_geom(w, h, panel_h);
            self.mount_fields(cx, rect, &g);
        }
    }

    fn overlay_body(&mut self, cx: &mut OverlayCx<'_, Vocab>) {
        match cx.id {}
    }

    fn intent(&mut self, intent: Intent<Vocab>, cx: &mut IntentCx<'_, Vocab>) {
        match intent {
            Intent::Window(WindowIntent::Opened { win, .. }) => {
                cx.command(AppCommand::Layout(LayoutCmd::SetChrome {
                    win,
                    visible: false,
                    height: 0.0,
                }));
                cx.command(AppCommand::Layout(LayoutCmd::OpenPanel {
                    win,
                    panel: Scene,
                    at: DockTarget::Root,
                }));
                cx.command(AppCommand::Window(WindowCmd::SetIcon {
                    win,
                    icon: Some(palm_icon(PALM_W, PALM)),
                }));
                self.ui.insert(
                    win,
                    WinUi {
                        menu: false,
                        ..WinUi::default()
                    },
                );
            }
            Intent::Window(WindowIntent::CloseRequested { win }) => {
                cx.command(AppCommand::Window(WindowCmd::Close(win)));
            }
            Intent::Unhandled(UnhandledInput::Key { key, .. }) => {
                if key.state == KeyState::Up {
                    return;
                }
                if key.code == KeyCode::Escape {
                    cx.command(AppCommand::Shutdown);
                }
            }
            Intent::Unhandled(UnhandledInput::Paste { text, .. }) => self.on_paste(&text),
            Intent::Text(TextIntent::Submitted { field, .. }) => {
                self.pending_submit = Some(field.as_str().to_string());
            }
            _ => {}
        }
    }
}

impl Oasis {
    fn on_bind_row(&mut self, x: f64, w: f64) {
        if self.engine.is_bound() {
            let unlink_x = menu::unlink_button_x(w);
            if x >= unlink_x {
                if self.unlink_armed {
                    self.engine.unlink();
                    self.unlink_armed = false;
                    self.bind_focused = false;
                    self.note.clear();
                } else {
                    self.unlink_armed = true;
                }
                return;
            }
            self.unlink_armed = false;
            if self.engine.cabinet_open() && x >= menu::cabinet_button_x(w) {
                crate::backend::open_marked_cabinet();
            }
            return;
        }
        self.unlink_armed = false;
        if x >= menu::bind_button_x(w) {
            self.submit_bind();
        } else if x >= 108.0 {
            self.bind_focused = true;
        } else {
            self.bind_focused = false;
        }
    }

    fn submit_bind(&mut self) {
        let text = self.bind_draft.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.engine.bind_claim(&text);
        self.bind_draft.clear();
        self.bind_focused = false;
        self.bind_reset_epoch = self.bind_reset_epoch.wrapping_add(1);
        if self.bind_reset_epoch == 0 {
            self.bind_reset_epoch = 1;
        }
    }

    /// Cabinet is a redeemed device. Standalone is a JSON entrance artefact.
    /// Anything else is not a connection, and the scene button does not start one.
    fn connection_ready(&self) -> bool {
        match self.product {
            Product::Cabinet => self.cabinet_ready(),
            Product::Standalone => self.standalone_artefact().is_some(),
        }
    }

    fn cabinet_ready(&self) -> bool {
        if self.engine.is_bound() {
            return true;
        }
        #[cfg(target_os = "ios")]
        {
            return crate::ios_host::cabinet_bound();
        }
        #[cfg(not(target_os = "ios"))]
        {
            return false;
        }
    }

    fn standalone_artefact(&self) -> Option<String> {
        let link = self.stand.link.trim();
        if json_object(link) {
            return Some(link.to_owned());
        }
        let path = std::env::current_exe()
            .ok()?
            .parent()?
            .join("standalone")
            .join("artefact.json");
        let text = std::fs::read_to_string(path).ok()?;
        let text = text.trim();
        if json_object(text) {
            Some(text.to_owned())
        } else {
            None
        }
    }

    fn raise_link(&mut self) -> bool {
        if !self.connection_ready() {
            return false;
        }
        if self.product == Product::Standalone {
            let Some(json) = self.standalone_artefact() else {
                return false;
            };
            if self.adopted != json {
                match self.engine.adopt_artefact(&json) {
                    Ok(()) => self.adopted = json,
                    Err(err) => {
                        #[cfg(not(target_os = "ios"))]
                        {
                            self.note = err;
                            return false;
                        }
                        #[cfg(target_os = "ios")]
                        {
                            let _ = err;
                            self.adopted = json;
                        }
                    }
                }
            }
        }
        self.engine.set_exit("auto");
        if self.engine.status().tun_block.is_some() {
            self.engine.set_listeners(true, false);
            self.fell_back = true;
        } else {
            self.engine.set_tun(true);
        }
        self.engine.connect();
        #[cfg(target_os = "ios")]
        {
            let arg = match self.product {
                Product::Standalone => self.standalone_artefact(),
                Product::Cabinet => None,
            };
            if !crate::ios_host::tunnel_start(arg.as_deref()) {
                return false;
            }
        }
        true
    }

    fn stop_link(&mut self) {
        self.engine.disconnect();
        self.fell_back = false;
        #[cfg(target_os = "ios")]
        crate::ios_host::tunnel_stop();
    }

    fn poll_scan(&mut self) {
        let outcome = self.scan.as_ref().map(|scan| scan.outcome());
        match outcome {
            Some(crate::camera::ScanOutcome::Done(raw)) => {
                self.scan = None;
                self.stand.accept_raw(&raw);
            }
            Some(crate::camera::ScanOutcome::Failed(err)) => {
                self.scan = None;
                self.note = err;
            }
            _ => {}
        }
    }

    /// A camera, file, clipboard, or Enter replaced the link. JSON connects.
    /// Anything else stays on the sheet as text and a QR.
    fn take_entrance(&mut self) -> bool {
        if !self.stand.take_relink() {
            return false;
        }
        self.note.clear();
        self.adopted.clear();
        self.sync_standalone_link();
        if !self.note.is_empty() {
            let err = self.note.clone();
            self.stand.say(&err);
        }
        let link = self.stand.link.trim();
        if self.product == Product::Standalone
            && !link.is_empty()
            && link == self.adopted
            && self.note.is_empty()
            && self.raise_link()
        {
            self.linked = true;
            self.target = 1.0;
            self.style = (self.toggles % 4) as f64;
            self.toggles = self.toggles.wrapping_add(1);
        }
        true
    }

    fn sync_standalone_link(&mut self) {
        if self.product != Product::Standalone {
            return;
        }
        let link = self.stand.link.trim().to_owned();
        if link.is_empty() || link == self.adopted {
            return;
        }
        if serde_json::from_str::<serde_json::Value>(&link).ok().filter(|v| v.is_object()).is_none() {
            return;
        }
        match self.engine.adopt_artefact(&link) {
            Ok(()) => {
                self.adopted = link;
                self.apply_scope();
            }
            Err(err) => {
                #[cfg(target_os = "ios")]
                {
                    let _ = err;
                    self.adopted = link;
                    self.apply_scope();
                }
                #[cfg(not(target_os = "ios"))]
                {
                    self.adopted = link;
                    self.note = err;
                }
            }
        }
    }

    /// Sheet height is the chrome band, the rows, and a 1px lip.
    fn sheet_panel_h(&self, window_h: f64) -> f64 {
        let body = if self.product == Product::Cabinet {
            let (_, content_h) = menu::layout_rows(&menu::cabinet_rows(), 0, 0, 0);
            content_h + if self.note.is_empty() { 0.0 } else { 26.0 }
        } else {
            stand::content_height(&self.stand, !self.note.is_empty())
        };
        (CHROME_H + body + LIP).min(window_h * menu::MAX_FRAC)
    }

    fn on_submit(&mut self, id: &str) {
        match id {
            "bind" => self.submit_bind(),
            "st-link" => self.stand.mark_relink(),
            "st-host" | "st-user" | "st-pass" => self.stand.install(),
            "st-site" => {
                self.stand.commit_site();
            }
            "st-app" => {
                self.stand.commit_app();
            }
            "st-dns" | "st-except" | "st-port" | "st-mtu" => self.stand.save_profile(),
            _ => {}
        }
        if self.stand.take_scope() {
            self.apply_scope();
        }
    }

    fn apply_scope(&mut self) {
        let (services, apps, bypass_services, bypass_apps) = self.stand.scope_args();
        self.engine.set_scope(&services, &apps, &bypass_services, &bypass_apps);
    }

    fn set_mode(&mut self, product: Product) {
        if self.product == product {
            return;
        }
        self.product = product;
        self.bind_focused = false;
        self.stand.focus = stand::Focus::None;
        let Some(path) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("client-mode")))
        else {
            return;
        };
        let text = match product {
            Product::Standalone => "standalone\n",
            Product::Cabinet => "cabinet\n",
        };
        let _ = std::fs::write(path, text);
    }

    fn sheet_open(&self) -> bool {
        self.ui.values().any(|ui| ui.menu)
    }

    fn on_paste(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() || !self.sheet_open() {
            return;
        }
        match self.product {
            Product::Cabinet if self.bind_focused => {
                if self.bind_draft.len() + text.len() <= 512 {
                    self.bind_draft.push_str(text);
                }
            }
            Product::Standalone => {
                let into_link = self.stand.focus == stand::Focus::None
                    || self.stand.focus == stand::Focus::Link
                    || text.starts_with('{');
                if into_link {
                    self.stand.set_link(text);
                    self.adopted.clear();
                } else {
                    self.stand.type_text(text);
                }
            }
            Product::Cabinet => {}
        }
    }

    fn register_bind(
        &mut self,
        coord: &mut uzor::input::InputCoordinator,
        view: &uzor_framework::VisualView<Vocab>,
        layer: &uzor::input::LayerId,
        at: &impl Fn(Rect) -> Rect,
        g: &menu::Geom,
        w: f64,
    ) {
        let (laid, _) = menu::layout_rows(&menu::cabinet_rows(), 0, 0, 0);
        let Some(row) = laid.iter().find(|lr| matches!(lr.row, menu::MenuRow::Bind)) else {
            return;
        };
        let ry = g.content.y + row.y;
        let field_h = 32.0;
        let by = ry + (row.h - field_h) / 2.0;
        if self.engine.is_bound() {
            let unlink_x = menu::unlink_button_x(w);
            let unlink_w = (w - 16.0 - unlink_x).max(0.0);
            let unlink = Rect::new(unlink_x, by, unlink_w, field_h);
            coord.register_atomic("bind-unlink", WidgetKind::Button, at(unlink), Sense::CLICK, layer);
            if view.clicked(&WidgetId::from("bind-unlink")) {
                if self.unlink_armed {
                    self.engine.unlink();
                    self.unlink_armed = false;
                    self.bind_focused = false;
                    self.note.clear();
                } else {
                    self.unlink_armed = true;
                }
            }
            if self.engine.cabinet_open() {
                let cabinet_x = menu::cabinet_button_x(w);
                let cabinet_w = (unlink_x - 6.0 - cabinet_x).max(0.0);
                let cabinet = Rect::new(cabinet_x, by, cabinet_w, field_h);
                coord.register_atomic(
                    "bind-cabinet",
                    WidgetKind::Button,
                    at(cabinet),
                    Sense::CLICK,
                    layer,
                );
                if view.clicked(&WidgetId::from("bind-cabinet")) {
                    crate::backend::open_marked_cabinet();
                }
            }
            return;
        }
        let button_x = menu::bind_button_x(w);
        let go = Rect::new(button_x, by, 60.0, field_h);
        coord.register_atomic("bind-go", WidgetKind::Button, at(go), Sense::CLICK, layer);
        if view.clicked(&WidgetId::from("bind-go")) {
            self.submit_bind();
        }
    }

    fn mount_fields(&mut self, cx: &mut PanelCx<'_, Vocab>, origin: Rect, g: &menu::Geom) {
        let at = |local: Rect| Rect::new(origin.x + local.x, origin.y + local.y, local.width, local.height);
        match self.product {
            Product::Cabinet => self.mount_bind(cx, &at, g),
            Product::Standalone => self.mount_stand(cx, &at, g),
        }
    }

    fn mount_bind(
        &mut self,
        cx: &mut PanelCx<'_, Vocab>,
        at: &impl Fn(Rect) -> Rect,
        g: &menu::Geom,
    ) {
        if self.engine.is_bound() {
            self.bind_focused = false;
            return;
        }
        let Some(local) = bind_field_local(g, g.panel.width) else {
            return;
        };
        let win = cx.window;
        let epoch = self.bind_reset_epoch;
        let reset = epoch_due(&mut self.bind_reset_seen, win, epoch);
        let (focused, _) = sync_field(
            cx,
            "bind",
            at(local),
            "claim or /c/ link",
            512,
            false,
            &mut self.bind_draft,
            reset,
            false,
        );
        self.bind_focused = focused;
    }

    fn mount_stand(
        &mut self,
        cx: &mut PanelCx<'_, Vocab>,
        at: &impl Fn(Rect) -> Rect,
        g: &menu::Geom,
    ) {
        let win = cx.window;
        let pass_epoch = self.stand.pass_epoch;
        let mut wipe_pass =
            pass_epoch != 0 && self.pass_reset_seen.get(&win).copied() != Some(pass_epoch);
        let clear_epoch = self.stand.clear_epoch;
        let clear_which = self.stand.clear_which;
        let mut wipe_clear =
            clear_epoch != 0 && self.clear_seen.get(&win).copied() != Some(clear_epoch);
        let specs = stand::fields(&self.stand, g.content);
        let link_epoch = self.stand.link_epoch();
        let mut force_link =
            link_epoch != 0 && self.link_force_seen.get(&win).copied() != Some(link_epoch);
        let mut focus = stand::Focus::None;
        let mut profile = false;
        let mut link_changed = false;
        for spec in specs {
            let mut reset = false;
            if spec.masked && wipe_pass {
                wipe_pass = false;
                self.pass_reset_seen.insert(win, pass_epoch);
                reset = true;
            }
            if wipe_clear && spec.focus == clear_which {
                wipe_clear = false;
                self.clear_seen.insert(win, clear_epoch);
                reset = true;
            }
            let overwrite = spec.focus == stand::Focus::Link && force_link;
            if overwrite {
                force_link = false;
                self.link_force_seen.insert(win, link_epoch);
            }
            let Some(value) = self.stand.field_mut(spec.focus) else {
                continue;
            };
            let (focused, changed) = sync_field(
                cx,
                spec.id,
                at(spec.rect),
                spec.placeholder,
                spec.max,
                spec.masked,
                value,
                reset,
                overwrite,
            );
            if focused {
                focus = spec.focus;
            }
            if changed {
                match spec.focus {
                    stand::Focus::Host
                    | stand::Focus::User
                    | stand::Focus::Port
                    | stand::Focus::Mtu
                    | stand::Focus::Dns
                    | stand::Focus::Except => profile = true,
                    stand::Focus::Link => link_changed = true,
                    _ => {}
                }
            }
        }
        self.stand.focus = focus;
        if profile {
            self.stand.save_profile();
        }
        if link_changed {
            self.adopted.clear();
            self.stand.save_link();
        }
    }
}

fn epoch_due(seen: &mut HashMap<WindowId, u64>, win: WindowId, epoch: u64) -> bool {
    if epoch == 0 || seen.get(&win).copied() == Some(epoch) {
        return false;
    }
    seen.insert(win, epoch);
    true
}

fn bind_field_local(g: &menu::Geom, w: f64) -> Option<Rect> {
    let (laid, _) = menu::layout_rows(&menu::cabinet_rows(), 0, 0, 0);
    let row = laid.iter().find(|lr| matches!(lr.row, menu::MenuRow::Bind))?;
    let ry = g.content.y + row.y;
    let field_h = 32.0;
    let field_y = ry + (row.h - field_h) / 2.0;
    let field_x = 108.0;
    let button_x = menu::bind_button_x(w);
    let field_w = (button_x - 8.0 - field_x).max(0.0);
    Some(Rect::new(field_x, field_y, field_w, field_h))
}

/// Push the model into the store while the field is idle. While it is focused,
/// the store is the text and the model follows it. `reset` clears both.
fn sync_field(
    cx: &mut PanelCx<'_, Vocab>,
    id: &str,
    rect: Rect,
    placeholder: &str,
    max: usize,
    masked: bool,
    value: &mut String,
    reset: bool,
    overwrite: bool,
) -> (bool, bool) {
    let wid = WidgetId::from(id);
    let push = !reset && !overwrite && !cx.view.is_focused(&wid);
    if reset {
        cx.widgets.coord.text_fields_mut().clear(&wid);
        value.clear();
    } else if overwrite || push {
        let differs = {
            let fields = cx.widgets.coord.text_fields();
            fields.has_field(&wid) && (overwrite || fields.text(&wid) != value.as_str())
        };
        if differs {
            cx.widgets.coord.text_fields_mut().set_text(&wid, value);
        }
    }
    let text_r = Rect::new(rect.x + 8.0, rect.y, (rect.width - 16.0).max(0.0), rect.height);
    let config = if masked {
        TextFieldConfig::password().with_max_len(max)
    } else {
        TextFieldConfig::text().with_max_len(max)
    };
    let resp = uzor_framework::widgets::kit::field(id, rect)
        .placeholder(placeholder)
        .font(14.0, "sans-serif")
        .colors("#F4F4F5", "#6B6F7A")
        .clip(true)
        .text_rect(text_r)
        .masked(masked)
        .config(config)
        .build(cx);
    if overwrite {
        cx.widgets.coord.text_fields_mut().set_text(&wid, value);
        return (resp.focused, false);
    }
    let changed = resp.focused && !reset && resp.text != *value;
    if changed {
        *value = resp.text;
    }
    (resp.focused, changed)
}

/// Open sheet from the top of the window. The chrome band is part of the
/// sheet fill. Mode buttons are painted on that fill.
fn open_geom(w: f64, h: f64, panel_h: f64) -> menu::Geom {
    menu::geom_at(w, h, panel_h, 1, 0.0, CHROME_H, LIP)
}

fn mode_pair(width: f64) -> (Rect, Rect) {
    let left = 4.0 + MARK + 8.0;
    let right = width - 6.0 - GLYPH * 2.0 - 6.0 - 8.0;
    let gap = 6.0;
    let h = 26.0;
    let y = (CHROME_H - h) / 2.0;
    let w = ((right - left - gap) / 2.0).max(0.0);
    (Rect::new(left, y, w, h), Rect::new(left + w + gap, y, w, h))
}

fn paint_modes(render: &mut dyn RenderContext, cab: Rect, solo: Rect, product: Product) {
    paint_mode(render, cab, "кабинет", product == Product::Cabinet, true);
    paint_mode(render, solo, "стенделон", product == Product::Standalone, false);
}

fn paint_mode(render: &mut dyn RenderContext, r: Rect, label: &str, on: bool, cabinet: bool) {
    let color = if on { "#FBB26A" } else { "#878B91" };
    render.set_stroke_color(color);
    render.set_stroke_width(1.25);
    render.set_line_dash(&[]);
    render.stroke_rect(r.x + 0.5, r.y + 0.5, (r.width - 1.0).max(0.0), (r.height - 1.0).max(0.0));
    let ix = r.x + 10.0;
    let iy = r.y + (r.height - 14.0) / 2.0;
    if cabinet {
        render.stroke_rect(ix + 1.0, iy + 4.0, 12.0, 10.0);
        render.begin_path();
        render.move_to(ix, iy + 5.0);
        render.line_to(ix + 7.0, iy);
        render.line_to(ix + 14.0, iy + 5.0);
        render.stroke();
    } else {
        render.stroke_rect(ix + 1.0, iy + 1.0, 12.0, 5.0);
        render.stroke_rect(ix + 1.0, iy + 8.0, 12.0, 5.0);
    }
    render.set_fill_color(color);
    render.set_font("13px sans-serif");
    render.set_text_align(TextAlign::Center);
    render.set_text_baseline(TextBaseline::Middle);
    render.fill_text(label, r.x + r.width / 2.0 + 6.0, r.y + r.height / 2.0);
}

fn menu_rect() -> Rect {
    Rect::new(4.0, (CHROME_H - MARK) / 2.0, MARK, MARK)
}

fn glyph_rect(width: f64, from_right: u32) -> Rect {
    let gap = 6.0;
    let y = (CHROME_H - GLYPH) / 2.0;
    let x = width - gap - GLYPH * (from_right as f64 + 1.0) - gap * from_right as f64;
    Rect::new(x, y, GLYPH, GLYPH)
}

fn palm_icon(px: u32, bytes: &[u8]) -> RgbaIcon {
    RgbaIcon::from_rgba(px, px, bytes.to_vec())
}

fn paint_menu(
    render: &mut dyn RenderContext,
    host: &JsAnimHost,
    scratch: &mut Vec<f64>,
    now: f64,
) -> Result<(), uzor::render::AnimScriptError> {
    let r = menu_rect();
    render.save();
    render.translate(r.x, r.y);
    let mut anim = host.anim("chrome");
    let painted = anim.paint(
        render,
        now,
        &AnimParams { w: r.width, h: r.height, seed: 3, mouse: None, args: [0.0; 8] },
        scratch,
    );
    render.restore();
    painted
}

fn paint_controls(render: &mut dyn RenderContext, width: f64, pointer: Option<(f64, f64)>) {
    let min_r = glyph_rect(width, 1);
    let close_r = glyph_rect(width, 0);
    let over_min = pointer.is_some_and(|(x, y)| min_r.contains(x, y));
    let over_close = pointer.is_some_and(|(x, y)| close_r.contains(x, y));
    if over_min {
        render.set_fill_color("rgba(255,255,255,0.10)");
        render.fill_rect(min_r.x, min_r.y, min_r.width, min_r.height);
    }
    paint_minus(render, min_r);
    if over_close {
        render.set_fill_color("#C42B1C");
        render.fill_rect(close_r.x, close_r.y, close_r.width, close_r.height);
    }
    paint_close_x(render, close_r, over_close);
}

fn paint_minus(render: &mut dyn RenderContext, r: Rect) {
    let cx = r.x + r.width / 2.0;
    let cy = r.y + r.height / 2.0;
    let hw = 5.0 * SCALE;
    let hh = 0.5 * SCALE;
    render.set_fill_color("rgba(0,0,0,0.65)");
    render.fill_rect(cx - hw - 0.6, cy - hh - 0.6, hw * 2.0 + 1.2, hh * 2.0 + 1.2);
    render.set_fill_color("#F3F3F3");
    render.fill_rect(cx - hw, cy - hh, hw * 2.0, hh * 2.0);
}

fn paint_close_x(render: &mut dyn RenderContext, r: Rect, hovered: bool) {
    let cx = r.x + r.width / 2.0;
    let cy = r.y + r.height / 2.0;
    let s = 5.0 * SCALE;
    if !hovered {
        stroke_x(render, cx, cy, s, "rgba(0,0,0,0.75)", 2.6 * SCALE);
    }
    let color = if hovered { "#FFFFFF" } else { "#F3F3F3" };
    stroke_x(render, cx, cy, s, color, 1.5 * SCALE);
}

fn stroke_x(render: &mut dyn RenderContext, cx: f64, cy: f64, s: f64, color: &str, width: f64) {
    render.set_stroke_color(color);
    render.set_stroke_width(width);
    render.set_line_dash(&[]);
    render.begin_path();
    render.move_to(cx - s, cy - s);
    render.line_to(cx + s, cy + s);
    render.stroke();
    render.begin_path();
    render.move_to(cx - s, cy + s);
    render.line_to(cx + s, cy - s);
    render.stroke();
}

pub fn launch() -> Result<(), Box<dyn std::error::Error>> {
    let host = JsAnimHost::new()?;
    host.load(include_str!("veg.js"))?;
    host.load(include_str!("figures.js"))?;
    host.load(include_str!("toon/toon-skin.js"))?;
    host.load(include_str!("toon/scene.js"))?;
    host.load(include_str!("chrome.js"))?;
    if !host.has("desert") {
        return Err("oasis: desert anim was not registered".into());
    }
    if !host.has("chrome") {
        return Err("oasis: chrome anim was not registered".into());
    }
    let (rider, rw, rh) = decode_png(include_bytes!("toon/rider-toon.png"))?;
    let (palm, pw, ph) = decode_png(include_bytes!("toon/palm-tall.png"))?;
    host.bind_bitmap("m1r4g3Skin", &rider, rw, rh, 0, true)?;
    host.bind_bitmap("m1r4g3PalmImage", &palm, pw, ph, 1, false)?;
    // One phone. Each window builds its own wgpu device and vello Renderer.
    let phone = app_window("phone", 390, 844, 16, 16);
    let cfg = RuntimeConfig {
        windows: vec![phone],
        tokens: Tokens::builtin(BuiltinSet::Dark),
        dark: true,
        ..RuntimeConfig::default()
    };
    run_native(
        Oasis {
            host,
            link: 1.0,
            target: 1.0,
            style: 0.0,
            toggles: 0,
            scratch: Vec::new(),
            paint_err: false,
            eased_at: f64::NAN,
            ui: HashMap::new(),
            product: product_mode(),
            engine: LocalHttp::start(),
            bind_draft: String::new(),
            bind_focused: false,
            unlink_armed: false,
            note: String::new(),
            fell_back: false,
            stand: Stand::load(),
            linked: false,
            adopted: String::new(),
            pending_submit: None,
            bind_reset_epoch: 0,
            bind_reset_seen: HashMap::new(),
            pass_reset_seen: HashMap::new(),
            clear_seen: HashMap::new(),
            link_force_seen: HashMap::new(),
            scan: None,
        },
        cfg,
        NativeOptions {
            // UrxWgpu draws fillRect as quads and drawImage as textured
            // quads or triangles. The GPU assembles the scene.
            render: RenderPreference {
                backend: Some(uzor::RenderBackend::UrxWgpu),
            },
            tray: Some(TraySpec {
                tooltip: Some("m1r4g3".into()),
                icon: Some(palm_icon(PALM_TRAY_W, PALM_TRAY)),
                items: Vec::new(),
            }),
            ..NativeOptions::default()
        },
    )?;
    Ok(())
}

fn json_object(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .is_some_and(|value| value.is_object())
}

/// Cabinet unless this launch passed `--standalone` or a previous launch wrote
/// `client-mode` beside the exe. The flag writes that file. Being unbound is
/// still cabinet.
fn product_mode() -> Product {
    let flag = std::env::args().skip(1).any(|arg| arg == "--standalone");
    let path = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join("client-mode")));
    if flag {
        if let Some(path) = &path {
            let _ = std::fs::write(path, "standalone\n");
        }
        return Product::Standalone;
    }
    if let Some(path) = path {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.trim() == "standalone" {
                return Product::Standalone;
            }
        }
    }
    Product::Cabinet
}

fn app_window(key: &str, w: u32, h: u32, x: i32, y: i32) -> WindowSpec {
    let mut window = WindowSpec::new(
        key,
        concat!("m1r4g3 ", env!("CARGO_PKG_VERSION")),
        SizePx::new(w, h),
    );
    window.decorations = false;
    window.corner_style = uzor::CornerStyle::Rounded;
    window.position = Some((x, y));
    window.min_inner_size = Some(SizePx::new(320, 320));
    window
}

fn decode_png(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), Box<dyn std::error::Error>> {
    let img = image::load_from_memory(bytes)?.into_rgba8();
    let (w, h) = img.dimensions();
    Ok((img.into_raw(), w, h))
}

fn local_clock() -> (f64, f64, f64) {
    #[cfg(windows)]
    {
        windows_clock()
    }
    #[cfg(not(windows))]
    {
        (12.0, 172.0, 0.0)
    }
}

#[cfg(windows)]
fn windows_clock() -> (f64, f64, f64) {
    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: SystemTime,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: SystemTime,
        daylight_bias: i32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(out: *mut SystemTime);
        fn GetTimeZoneInformation(out: *mut TimeZoneInformation) -> u32;
    }

    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    let mut now = SystemTime {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    let mut zone = std::mem::MaybeUninit::<TimeZoneInformation>::uninit();
    let (hour, day, dst) = unsafe {
        GetLocalTime(&mut now);
        let kind = GetTimeZoneInformation(zone.as_mut_ptr());
        let hour = now.hour as f64 + now.minute as f64 / 60.0 + now.second as f64 / 3600.0;
        let dst = if kind == TIME_ZONE_ID_DAYLIGHT { 1.0 } else { 0.0 };
        (hour, day_of_year(now.year, now.month, now.day), dst)
    };
    (hour, day, dst)
}

#[cfg(windows)]
fn day_of_year(year: u16, month: u16, day: u16) -> f64 {
    const BEFORE: [u16; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let m = (month as usize).clamp(1, 12);
    let mut n = BEFORE[m - 1] as u32 + day as u32;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    if leap && month > 2 {
        n += 1;
    }
    n as f64
}
