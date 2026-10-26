//! iOS frame pump. Swift owns the UIView and the packet tunnel. This module
//! paints the same `MirageClientApp` the desktop window paints.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use uzor::core::types::Rect;
use uzor::render::Painter;
use uzor::framework::app::{App, NoPanel};
use uzor::framework::multi_window::WindowCtx;
use uzor::framework::render_control::RenderControl;
use uzor::input::{KeyCode, ModifierKeys, MouseButton, PlatformEvent};
use uzor::layout::window::{RawHandle, WindowKey, WindowProvider};
use uzor::layout::LayoutManager;
use uzor::platform::types::RenderBackend;
use uzor_render_tiny_skia::TinySkiaCpuRenderContext;

use crate::app::MirageClientApp;
use crate::backend::{spawn_cabinet_countries, EngineLink, EngineStatus};
use crate::centerpiece::SkinKind;

struct Hooks {
    start: Option<extern "C" fn()>,
    stop: Option<extern "C" fn()>,
    status: Option<extern "C" fn() -> i32>,
    bind: Option<extern "C" fn(*const c_char)>,
    unlink: Option<extern "C" fn()>,
}

fn hooks() -> &'static Mutex<Hooks> {
    static HOOKS: OnceLock<Mutex<Hooks>> = OnceLock::new();
    HOOKS.get_or_init(|| {
        Mutex::new(Hooks {
            start: None,
            stop: None,
            status: None,
            bind: None,
            unlink: None,
        })
    })
}

struct StoreHooks {
    status: Arc<Mutex<EngineStatus>>,
    bound: Arc<AtomicBool>,
    name: Arc<Mutex<Option<String>>>,
    note: Arc<Mutex<Option<String>>>,
}

impl StoreHooks {
    fn start() -> Self {
        let status = Arc::new(Mutex::new(EngineStatus {
            mode: "tun".to_owned(),
            exit: "auto".to_owned(),
            ..EngineStatus::default()
        }));
        spawn_cabinet_countries(status.clone());
        Self {
            status,
            bound: Arc::new(AtomicBool::new(false)),
            name: Arc::new(Mutex::new(None)),
            note: Arc::new(Mutex::new(None)),
        }
    }
}

impl EngineLink for StoreHooks {
    fn status(&self) -> EngineStatus {
        let code = hooks()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .status
            .map(|fun| fun())
            .unwrap_or(0);
        let mut guard = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        match code {
            2 => {
                guard.phase = "connected".to_owned();
                guard.tun_alive = true;
                guard.tun = true;
                guard.online = true;
            }
            1 => {
                guard.phase = "connecting".to_owned();
                guard.tun_alive = false;
                guard.tun = false;
                guard.online = true;
            }
            _ => {
                guard.phase = "off".to_owned();
                guard.tun_alive = false;
                guard.tun = false;
            }
        }
        guard.clone()
    }

    fn connect(&self) {
        if !self.is_bound() {
            return;
        }
        {
            let mut guard = self.status.lock().unwrap_or_else(PoisonError::into_inner);
            guard.phase = "connecting".to_owned();
            guard.mode = "tun".to_owned();
        }
        if let Some(fun) = hooks().lock().unwrap_or_else(PoisonError::into_inner).start {
            fun();
        }
    }

    fn disconnect(&self) {
        if let Some(fun) = hooks().lock().unwrap_or_else(PoisonError::into_inner).stop {
            fun();
        }
    }

    fn set_listeners(&self, socks: bool, http: bool) {
        let mut guard = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        guard.socks = socks && !http;
        guard.http = http && !socks;
        guard.tun = false;
        guard.mode = if guard.http { "http" } else { "socks" }.to_owned();
    }

    fn set_tun(&self, enabled: bool) {
        let mut guard = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        guard.socks = false;
        guard.http = false;
        guard.tun = false;
        guard.mode = if enabled { "tun" } else { "socks" }.to_owned();
    }

    fn set_exit(&self, exit: &str) {
        self.status.lock().unwrap_or_else(PoisonError::into_inner).exit = exit.to_owned();
    }

    fn set_scope(
        &self,
        services: &[String],
        apps: &[String],
        bypass_services: &[String],
        bypass_apps: &[String],
    ) {
        let mut guard = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        guard.services = services.to_vec();
        guard.apps = apps.to_vec();
        guard.bypass_services = bypass_services.to_vec();
        guard.bypass_apps = bypass_apps.to_vec();
    }

    fn bind_claim(&self, text: &str) {
        let Ok(c) = CString::new(text.trim()) else {
            self.note.lock().unwrap_or_else(PoisonError::into_inner).replace("код не принят".to_owned());
            return;
        };
        if let Some(fun) = hooks().lock().unwrap_or_else(PoisonError::into_inner).bind {
            fun(c.as_ptr());
        }
    }

    fn is_bound(&self) -> bool {
        self.bound.load(Ordering::Relaxed)
    }

    fn bound_name(&self) -> Option<String> {
        self.name.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn cabinet_open(&self) -> bool {
        false
    }

    fn unlink(&self) {
        self.bound.store(false, Ordering::Relaxed);
        *self.name.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let hooks = hooks().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(fun) = hooks.unlink {
            fun();
        } else if let Some(fun) = hooks.stop {
            fun();
        }
    }

    fn take_bind_note(&self) -> Option<String> {
        self.note.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

struct HeadlessWindow {
    rect: Rect,
    scale: f64,
}

impl WindowProvider for HeadlessWindow {
    fn poll_events(&mut self) -> Vec<PlatformEvent> {
        Vec::new()
    }
    fn window_rect(&self) -> Rect {
        self.rect
    }
    fn scale_factor(&self) -> f64 {
        self.scale
    }
    fn request_redraw(&mut self) {}
    fn should_close(&self) -> bool {
        false
    }
    fn raw_window_handle(&self) -> Option<RawHandle> {
        None
    }
}

struct MockRenderControl;

impl RenderControl for MockRenderControl {
    fn active_backend(&self) -> RenderBackend {
        RenderBackend::TinySkia
    }
    fn available_backends(&self) -> Vec<RenderBackend> {
        vec![RenderBackend::TinySkia]
    }
    fn set_backend(&mut self, _b: RenderBackend) {}
    fn fps_limit(&self) -> u32 {
        0
    }
    fn set_fps_limit(&mut self, _fps: u32) {}
    fn msaa_samples(&self) -> u8 {
        0
    }
    fn set_msaa_samples(&mut self, _n: u8) {}
    fn vsync(&self) -> bool {
        false
    }
    fn set_vsync(&mut self, _on: bool) {}
}

enum Ptr {
    Down(f64, f64),
    Move(f64, f64),
    Up(f64, f64),
    Cancel(f64, f64),
}

struct Host {
    app: MirageClientApp,
    layout: LayoutManager<NoPanel>,
    key: WindowKey,
    width: f64,
    height: f64,
    dpr: f64,
    entered: bool,
    pointer: VecDeque<Ptr>,
    bound: Arc<AtomicBool>,
    name: Arc<Mutex<Option<String>>>,
    note: Arc<Mutex<Option<String>>>,
}

impl Host {
    fn new() -> Self {
        let engine = StoreHooks::start();
        let bound = engine.bound.clone();
        let name = engine.name.clone();
        let note = engine.note.clone();
        Self {
            app: MirageClientApp::new(SkinKind::Planet, Box::new(engine)),
            layout: LayoutManager::new(),
            key: WindowKey::new("main"),
            width: 0.0,
            height: 0.0,
            dpr: 1.0,
            entered: false,
            pointer: VecDeque::new(),
            bound,
            name,
            note,
        }
    }

    fn attach(&mut self, width: f64, height: f64, dpr: f64) {
        self.layout = LayoutManager::new();
        let rect = Rect::new(0.0, 0.0, width, height);
        self.layout.attach_window(
            self.key.clone(),
            Box::new(HeadlessWindow { rect, scale: dpr }),
        );
        self.layout.set_current_window(self.key.clone());
        self.layout.solve(rect);
        self.width = width;
        self.height = height;
        self.dpr = dpr;
    }

    fn apply_pointer(&mut self) {
        while let Some(ev) = self.pointer.pop_front() {
            if !self.entered {
                self.entered = true;
                App::on_event(&mut self.app, &PlatformEvent::PointerEntered);
            }
            match ev {
                Ptr::Move(x, y) => {
                    self.layout.on_pointer_move(x, y);
                    App::on_event(&mut self.app, &PlatformEvent::PointerMoved { x, y });
                }
                Ptr::Down(x, y) => {
                    self.layout.on_pointer_move(x, y);
                    App::on_event(&mut self.app, &PlatformEvent::PointerMoved { x, y });
                    self.layout.on_pointer_down(x, y);
                    App::on_event(
                        &mut self.app,
                        &PlatformEvent::PointerDown { x, y, button: MouseButton::Left },
                    );
                }
                Ptr::Up(x, y) => {
                    self.layout.on_pointer_move(x, y);
                    let _ = self.layout.on_pointer_up(x, y);
                    App::on_event(
                        &mut self.app,
                        &PlatformEvent::PointerUp { x, y, button: MouseButton::Left },
                    );
                }
                Ptr::Cancel(x, y) => {
                    self.layout.on_pointer_move(x, y);
                    let _ = self.layout.on_pointer_up(x, y);
                    App::on_event(
                        &mut self.app,
                        &PlatformEvent::PointerUp { x, y, button: MouseButton::Left },
                    );
                    self.entered = false;
                    App::on_event(&mut self.app, &PlatformEvent::PointerLeft);
                }
            }
        }
    }
}

fn paint(
    app: &mut MirageClientApp,
    layout: &mut LayoutManager<NoPanel>,
    key: &WindowKey,
    ctx: &mut TinySkiaCpuRenderContext,
    rect: Rect,
) {
    let mut rc = MockRenderControl;
    let mut win = WindowCtx {
        key,
        layout,
        render: ctx,
        rect,
        render_control: &mut rc,
    };
    app.ui(&mut win);
}

thread_local! {
    static HOST: RefCell<Host> = RefCell::new(Host::new());
}

#[no_mangle]
pub extern "C" fn mirage_gui_frame(width_px: u32, height_px: u32, dpr: f64, pixels: *mut u8) -> i32 {
    if width_px == 0 || height_px == 0 || pixels.is_null() {
        return 1;
    }
    let dpr = if dpr.is_finite() && dpr >= 1.0 { dpr } else { 1.0 };
    let logical_w = width_px as f64 / dpr;
    let logical_h = height_px as f64 / dpr;
    HOST.with(|slot| {
        let mut host = slot.borrow_mut();
        if (host.width - logical_w).abs() > 0.5
            || (host.height - logical_h).abs() > 0.5
            || (host.dpr - dpr).abs() > 0.01
        {
            host.attach(logical_w, logical_h, dpr);
        }
        host.apply_pointer();
        let rect = Rect::new(0.0, 0.0, logical_w, logical_h);
        host.layout.solve(rect);
        let mut ctx = TinySkiaCpuRenderContext::new(width_px, height_px, dpr);
        ctx.clear(tiny_skia::Color::from_rgba8(0x0B, 0x0B, 0x0E, 0xFF));
        // Layout is in points. The pixmap is physical pixels, same split the
        // desktop surface uses between the window and the framebuffer.
        ctx.scale(dpr, dpr);
        // RefMut does not split field borrows across one call. Reborrow the Host.
        {
            let h = &mut *host;
            paint(&mut h.app, &mut h.layout, &h.key, &mut ctx, rect);
        }
        let src = ctx.pixels();
        let need = (width_px as usize).saturating_mul(height_px as usize).saturating_mul(4);
        if src.len() < need {
            return 1;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), pixels, need);
        }
        0
    })
}

#[no_mangle]
pub extern "C" fn mirage_gui_pointer(phase: i32, x: f64, y: f64) {
    HOST.with(|slot| {
        let ev = match phase {
            0 => Ptr::Down(x, y),
            1 => Ptr::Move(x, y),
            2 => Ptr::Up(x, y),
            _ => Ptr::Cancel(x, y),
        };
        slot.borrow_mut().pointer.push_back(ev);
    });
}

#[no_mangle]
pub extern "C" fn mirage_gui_text(utf8: *const c_char) {
    if utf8.is_null() {
        return;
    }
    let text = unsafe { CStr::from_ptr(utf8) }.to_string_lossy().into_owned();
    if text.is_empty() {
        return;
    }
    HOST.with(|slot| {
        App::on_event(
            &mut slot.borrow_mut().app,
            &PlatformEvent::TextInput { text },
        );
    });
}

#[no_mangle]
pub extern "C" fn mirage_gui_key(key: i32) {
    let key = match key {
        1 => KeyCode::Backspace,
        2 => KeyCode::Enter,
        3 => KeyCode::Escape,
        _ => return,
    };
    HOST.with(|slot| {
        App::on_event(
            &mut slot.borrow_mut().app,
            &PlatformEvent::KeyDown { key, modifiers: ModifierKeys::none() },
        );
    });
}

#[no_mangle]
pub extern "C" fn mirage_gui_wants_keyboard() -> i32 {
    HOST.with(|slot| i32::from(slot.borrow().app.wants_keyboard()))
}

#[no_mangle]
pub extern "C" fn mirage_gui_bind_result(utf8: *const c_char) {
    let text = if utf8.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(utf8) }.to_string_lossy().into_owned();
        if s.is_empty() { None } else { Some(s) }
    };
    HOST.with(|slot| {
        *slot.borrow().note.lock().unwrap_or_else(PoisonError::into_inner) = text;
    });
}

#[no_mangle]
pub extern "C" fn mirage_gui_set_bound(bound: i32, name: *const c_char) {
    let label = if name.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned())
    };
    HOST.with(|slot| {
        let host = slot.borrow();
        host.bound.store(bound != 0, Ordering::Relaxed);
        *host.name.lock().unwrap_or_else(PoisonError::into_inner) = label.filter(|s| !s.is_empty());
    });
}

#[no_mangle]
pub extern "C" fn mirage_gui_set_hooks(
    start: Option<extern "C" fn()>,
    stop: Option<extern "C" fn()>,
    status: Option<extern "C" fn() -> i32>,
    bind: Option<extern "C" fn(*const c_char)>,
) {
    let mut guard = hooks().lock().unwrap_or_else(PoisonError::into_inner);
    guard.start = start;
    guard.stop = stop;
    guard.status = status;
    guard.bind = bind;
}

#[no_mangle]
pub extern "C" fn mirage_gui_set_unlink(unlink: Option<extern "C" fn()>) {
    hooks().lock().unwrap_or_else(PoisonError::into_inner).unlink = unlink;
}

fn redeem_claim(code: &str) -> Result<String, String> {
    let code = code.trim();
    if code.is_empty() || code.len() > 512 {
        return Err("empty".to_owned());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| "transport".to_owned())?;
    let origin = "https://panel.m1r4g3.bid";
    let (status, body) = post_json(&client, &format!("{origin}/public/claims/redeem"), serde_json::json!({ "claim": code }))?;
    if status != 200 && status != 202 {
        return Err("refused".to_owned());
    }
    let poll = body.get("poll").and_then(|value| value.as_str()).ok_or_else(|| "refused".to_owned())?.to_owned();
    let started = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(90);
    loop {
        if started.elapsed() >= budget {
            return Err("pending".to_owned());
        }
        let (status, body) = post_json(&client, &format!("{origin}/public/claims/poll"), serde_json::json!({ "poll": poll }))?;
        if status == 429 {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        if status != 200 && status != 202 {
            return Err("refused".to_owned());
        }
        let state = body.get("status").and_then(|value| value.as_str()).unwrap_or("");
        if state == "ready" {
            let document = body
                .get("artefact")
                .cloned()
                .filter(|value| !value.is_null())
                .or_else(|| body.get("packet").cloned().filter(|value| !value.is_null()))
                .ok_or_else(|| "empty-artefact".to_owned())?;
            return serde_json::to_string(&document).map_err(|_| "refused".to_owned());
        }
        if state == "pending" {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        return Err("refused".to_owned());
    }
}

fn post_json(client: &reqwest::blocking::Client, url: &str, body: serde_json::Value) -> Result<(u16, serde_json::Value), String> {
    let response = client.post(url).json(&body).send().map_err(|_| "transport".to_owned())?;
    let status = response.status().as_u16();
    let text = response.text().map_err(|_| "transport".to_owned())?;
    let value = if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
    };
    Ok((status, value))
}

#[no_mangle]
pub unsafe extern "C" fn mirage_claim_redeem(code: *const c_char) -> *mut c_char {
    if code.is_null() {
        return std::ptr::null_mut();
    }
    let code = match unsafe { CStr::from_ptr(code) }.to_str() {
        Ok(code) => code.to_owned(),
        Err(_) => return std::ptr::null_mut(),
    };
    let json = match redeem_claim(&code) {
        Ok(json) => json,
        Err(_) => return std::ptr::null_mut(),
    };
    CString::new(json).map(CString::into_raw).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn mirage_store_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}
