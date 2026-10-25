//! m1r4g3 desktop shell. The oasis scene fills the window. Chrome is a
//! transparent overlay: a stroked amber palm (no fill) on the left, and
//! minimize / close on the right drawn like a system caption. The empty
//! band drags the window through uzor's floating controls; no strip is painted.
//! An open sheet starts at the top of the window, so its tab row is the
//! chrome band. A click below the band eases the link over 0.55s.

use std::collections::HashMap;

use uzor::input::keyboard::KeyCode;
use uzor::input::{Sense, WidgetKind};
use uzor::layout::docking::DockPanel;
use uzor::render::{AnimParams, AnimScript, RenderContext};
use uzor::tokens::{BuiltinSet, Tokens};
use uzor::{Rect, RgbaIcon, WidgetId};
use uzor_anim_js::JsAnimHost;
use uzor_framework::host::native::{NativeOptions, TraySpec, run_native};
use uzor_framework::{
    App, AppCommand, ChromeModel, ControlsPlacement, DockTarget, FloatingControls, Intent, IntentCx,
    KeyInput, KeyState, LayoutCmd, OverlayCx, OverlayModel, PanelCx, PanelHome, RuntimeConfig, SizePx,
    Spec, UnhandledInput, WindowCmd, WindowControlsStyle, WindowId, WindowIntent, WindowSpec,
};

use crate::backend::{EngineLink, LocalHttp};
use crate::centerpiece::SkinKind;
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
    /// Last standalone link handed to the subscriber. Stops a re-spawn every frame.
    adopted: String,
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
        self.sync_standalone_link();

        let mut ui = self.ui.get(&win).cloned().unwrap_or_default();
        let panel_h = self.sheet_panel_h(h);
        let g = menu::geom_at(w, h, panel_h, 1, 0.0, CHROME_H);

        if ui.menu {
            // Nothing is registered on the chrome band except the palm and the
            // caption buttons, so the band stays a window drag.
            let bottom = g.panel.y + g.panel.height;
            if bottom < h {
                hit(cx.widgets.coord, "scrim", at(Rect::new(0.0, bottom, w, (h - bottom).max(0.0))));
            }
            if self.product == Product::Cabinet {
                let rows = menu::cabinet_rows();
                let (laid, _) = menu::layout_rows(&rows, 0, 0, 0);
                if let Some(lr) = laid.first() {
                    let ry = g.content.y + lr.y;
                    hit(cx.widgets.coord, "row:0", at(Rect::new(0.0, ry, w, lr.h)));
                    if cx.view.clicked(&WidgetId::from("row:0")) {
                        let x = cx.view.raw_pointer().map(|(px, _)| px - rect.x).unwrap_or(0.0);
                        self.on_bind_row(x, w);
                    }
                }
            } else {
                stand::register(cx.widgets.coord, &cx.view, &layer, &at, &g, &mut self.stand);
            }
            if cx.view.clicked(&WidgetId::from("scrim")) {
                ui.menu = false;
                self.bind_focused = false;
            }
        } else {
            hit(cx.widgets.coord, HIT_SCENE, at(Rect::new(0.0, CHROME_H, w, scene_h)));
        }
        hit(cx.widgets.coord, &id_menu, at(menu_r));
        hit(cx.widgets.coord, &id_min, at(min_r));
        hit(cx.widgets.coord, &id_close, at(close_r));
        let menu_click = cx.view.clicked(&WidgetId::from(id_menu.as_str()));
        let min_click = cx.view.clicked(&WidgetId::from(id_min.as_str()));
        let close_click = cx.view.clicked(&WidgetId::from(id_close.as_str()));
        if !ui.menu
            && cx.view.clicked(&WidgetId::from(HIT_SCENE))
            && !menu_click
            && !min_click
            && !close_click
        {
            let on = self.target < 0.5;
            self.target = if on { 1.0 } else { 0.0 };
            self.style = (self.toggles % 6) as f64;
            self.toggles = self.toggles.wrapping_add(1);
            if on {
                self.raise_link();
            } else {
                self.engine.disconnect();
                self.fell_back = false;
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
            let g = menu::geom_at(w, h, panel_h, 1, 0.0, CHROME_H);
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
                    SkinKind::Planet,
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
                    self.bind_focused,
                    &self.bind_draft,
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
            } else {
                let note = if self.note.is_empty() { None } else { Some(self.note.as_str()) };
                stand::paint(render, &g, &self.stand, false, note);
            }
        }
        if let Err(err) = paint_menu(render, &self.host, &mut self.scratch, now) {
            if !self.paint_err {
                eprintln!("menu paint: {err}");
                self.paint_err = true;
            }
        }
        paint_controls(render, w, pointer);
        render.restore();
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
                    return;
                }
                self.on_key(&key);
            }
            Intent::Unhandled(UnhandledInput::Paste { text, .. }) => self.on_paste(&text),
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
    }

    fn raise_link(&mut self) {
        self.engine.set_exit("auto");
        if self.engine.status().tun_block.is_some() {
            self.engine.set_listeners(true, false);
            self.fell_back = true;
        } else {
            self.engine.set_tun(true);
        }
        self.engine.connect();
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
            Ok(()) => self.adopted = link,
            Err(err) => {
                self.adopted = link;
                self.note = err;
            }
        }
    }

    /// Sheet height is the chrome band plus the rows plus the lip. It does not
    /// grow past that content, and it does not keep a fraction of the window.
    fn sheet_panel_h(&self, window_h: f64) -> f64 {
        let body = if self.product == Product::Cabinet {
            let (_, content_h) = menu::layout_rows(&menu::cabinet_rows(), 0, 0, 0);
            content_h + if self.note.is_empty() { 0.0 } else { 26.0 }
        } else {
            stand::content_height(!self.stand.link.is_empty())
        };
        (CHROME_H + body + menu::SEP_H).min(window_h * menu::MAX_FRAC)
    }

    fn sheet_open(&self) -> bool {
        self.ui.values().any(|ui| ui.menu)
    }

    fn on_key(&mut self, key: &KeyInput) {
        if !self.sheet_open() {
            return;
        }
        if key.code == KeyCode::Backspace {
            if self.product == Product::Cabinet && self.bind_focused {
                self.bind_draft.pop();
            } else if self.product == Product::Standalone {
                self.stand.backspace();
                if self.stand.focus == stand::Focus::Link {
                    self.stand.save_link();
                    self.adopted.clear();
                }
            }
            return;
        }
        if key.code == KeyCode::Enter {
            if self.product == Product::Cabinet && self.bind_focused {
                self.submit_bind();
            } else if self.product == Product::Standalone {
                if self.stand.focus == stand::Focus::Link {
                    self.stand.save_link();
                    self.stand.focus = stand::Focus::None;
                } else {
                    self.stand.install();
                }
            }
            return;
        }
        let Some(text) = key.text.as_deref() else {
            return;
        };
        if text.chars().any(|c| c.is_control()) {
            return;
        }
        if self.product == Product::Cabinet && self.bind_focused {
            if self.bind_draft.len() + text.len() <= 512 {
                self.bind_draft.push_str(text);
            }
        } else if self.product == Product::Standalone {
            let editing_link = self.stand.focus == stand::Focus::Link;
            self.stand.type_text(text);
            if editing_link {
                self.adopted.clear();
            }
        }
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
    host.load(include_str!("scene.js"))?;
    host.load(include_str!("chrome.js"))?;
    if !host.has("desert") {
        return Err("oasis: desert anim was not registered".into());
    }
    if !host.has("chrome") {
        return Err("oasis: chrome anim was not registered".into());
    }
    // Logical points, 1:1 on this 1920×1080 desktop. iPhone 14 / 16e is
    // 390×844 (9:19.5). iPad 9.7" is 768×1024 (3:4). An 11" iPad is
    // 820×1180 and does not fit above the taskbar.
    let phone = app_window("phone", 390, 844, 24, 96);
    let ipad = app_window("ipad", 768, 1024, 438, 8);
    let cfg = RuntimeConfig {
        windows: vec![phone, ipad],
        tokens: Tokens::builtin(BuiltinSet::Dark),
        dark: true,
        ..RuntimeConfig::default()
    };
    run_native(
        Oasis {
            host,
            link: 0.0,
            target: 0.0,
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
            adopted: String::new(),
        },
        cfg,
        NativeOptions {
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
    window.min_inner_size = Some(SizePx::new(320, 520));
    window
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
