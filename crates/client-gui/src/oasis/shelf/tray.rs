//! The tray icon this process owns.
//! uzor's tray builds the icon and the labels once, so a language change
//! and a theme change never reach it. This one is created on the first
//! UI tick and updates both.
//!
//! The menu is one action (подключиться / отключиться) and quit.
//! The hover tip is the app name and version. The palm color is the state.
//! A state change opens a small window beside the tray icon: the palm and
//! one line, for two seconds. The colored palm means the link came up, the
//! gray palm means it went down. The Windows toast is not used: its header
//! (the exe icon, the product name, and the system buttons) cannot be
//! removed. "Disconnected" is sent only after a real link.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
#[cfg(windows)]
use std::time::{Duration, Instant};

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use super::{post, Cmd};

const PALM: &[u8] = include_bytes!("../palm64.rgba");
const SIDE: u32 = 64;
const CAPTION: &str = concat!("m1r4g3 ", env!("CARGO_PKG_VERSION"));

struct Live {
    tray: TrayIcon,
    link: MenuItem,
    quit: MenuItem,
    lang: u8,
    ink: u32,
    up: bool,
    /// Last state a balloon was shown for. `None` until the first tick,
    /// so a start that is already down does not announce "disconnected".
    told: Option<bool>,
    /// Frames to wait before retrying a balloon the shell did not accept.
    balloon_wait: u8,
}

thread_local! {
    static LIVE: RefCell<Option<Live>> = const { RefCell::new(None) };
}

static BUSY: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);

struct Busy;
impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::SeqCst);
    }
}

pub fn tick(up: bool) {
    if FAILED.load(Ordering::SeqCst) || BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    let _busy = Busy;
    let ready = LIVE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = open(up);
        }
        if let Some(live) = slot.as_mut() {
            refresh(live, up);
            true
        } else {
            false
        }
    });
    if ready {
        pump();
        #[cfg(windows)]
        expire_tip();
    } else {
        FAILED.store(true, Ordering::SeqCst);
    }
}

fn open(up: bool) -> Option<Live> {
    #[cfg(windows)]
    super::wincolor::allow_dark_menus();
    let rgb = paint(up);
    let icon = match Icon::from_rgba(palm(rgb), SIDE, SIDE) {
        Ok(icon) => icon,
        Err(err) => {
            tracing::warn!("tray shelf: icon: {err}");
            return None;
        }
    };
    let link = MenuItem::with_id("toggle", action(up), true, None);
    let quit = MenuItem::with_id("quit", crate::text::tr("shelf.quit"), true, None);
    let menu = Menu::new();
    if menu.append(&link).is_err() || menu.append(&quit).is_err() {
        tracing::warn!("tray shelf: menu failed");
        return None;
    }
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(CAPTION)
        .with_menu_on_left_click(false)
        .with_icon(icon)
        .with_icon_as_template(cfg!(target_os = "macos") && up)
        .build();
    let tray = match tray {
        Ok(tray) => tray,
        Err(err) => {
            tracing::warn!("tray shelf: {err}");
            return None;
        }
    };
    tracing::info!("tray shelf");
    Some(Live {
        tray,
        link,
        quit,
        lang: crate::text::current() as u8,
        ink: pack(rgb),
        up,
        told: None,
        balloon_wait: 0,
    })
}

fn refresh(live: &mut Live, up: bool) {
    let lang = crate::text::current() as u8;
    if live.lang != lang || live.up != up {
        live.lang = lang;
        live.up = up;
        live.link.set_text(action(up));
        live.quit.set_text(crate::text::tr("shelf.quit"));
    }
    announce(live, up);
    let rgb = paint(up);
    let ink = pack(rgb);
    if live.ink != ink {
        live.ink = ink;
        if let Ok(icon) = Icon::from_rgba(palm(rgb), SIDE, SIDE) {
            #[cfg(target_os = "macos")]
            {
                let _ = live.tray.set_icon_with_as_template(Some(icon), up);
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = live.tray.set_icon(Some(icon));
            }
        }
    }
}

fn pump() {
    let receiver = MenuEvent::receiver();
    while let Ok(event) = receiver.try_recv() {
        let cmd = if event.id == "quit" {
            Some(Cmd::Quit)
        } else if event.id == "toggle" || event.id == "connect" || event.id == "disconnect" {
            Some(Cmd::Toggle)
        } else {
            None
        };
        if let Some(cmd) = cmd {
            tracing::info!("shelf tray {}", event.id.0);
            post(cmd);
        }
    }
}

fn action(up: bool) -> &'static str {
    if up {
        crate::text::tr("shelf.disconnect")
    } else {
        crate::text::tr("shelf.connect")
    }
}

fn state_tip(up: bool) -> &'static str {
    if up {
        crate::text::tr("shelf.state.on")
    } else {
        crate::text::tr("shelf.state.off")
    }
}

/// Amnezia's `NotificationHandler::setConnectionState`: a balloon on the
/// way up, and on the way down only when the link had actually been up.
fn announce(live: &mut Live, up: bool) {
    if live.told == Some(up) {
        return;
    }
    // Amnezia skips the disconnected balloon unless the link had been up.
    if !up && live.told != Some(true) {
        live.told = Some(false);
        return;
    }
    if live.balloon_wait > 0 {
        live.balloon_wait -= 1;
        return;
    }
    if balloon(up) {
        live.told = Some(up);
        tracing::info!("tray balloon {}", if up { "up" } else { "down" });
    } else {
        live.balloon_wait = 30;
    }
}

fn balloon(up: bool) -> bool {
    let line = state_tip(up);
    #[cfg(windows)]
    return win_balloon(line, up);
    #[cfg(target_os = "linux")]
    {
        linux_balloon(line);
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        mac_balloon(line);
        return true;
    }
}

#[cfg(target_os = "linux")]
fn linux_balloon(line: &str) {
    // One line. The desktop notification, same role as showMessage.
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "m1r4g3", "-t", "2000", line])
        .spawn();
}

#[cfg(target_os = "macos")]
fn mac_balloon(line: &str) {
    // Amnezia posts a UserNotifications request. This is the system
    // notification without linking that framework. The line is the title
    // so Notification Center does not add a second name above it.
    let script = format!("display notification \"\" with title \"{}\"", apple(line));
    let _ = std::process::Command::new("osascript").args(["-e", &script]).spawn();
}

#[cfg(target_os = "macos")]
fn apple(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn palm(rgb: [u8; 3]) -> Vec<u8> {
    let mut out = vec![0u8; PALM.len()];
    for (dst, src) in out.chunks_exact_mut(4).zip(PALM.chunks_exact(4)) {
        dst[0] = rgb[0];
        dst[1] = rgb[1];
        dst[2] = rgb[2];
        dst[3] = src[3];
    }
    out
}

fn pack(rgb: [u8; 3]) -> u32 {
    (rgb[0] as u32) << 16 | (rgb[1] as u32) << 8 | rgb[2] as u32
}

/// Off is gray. On is the taskbar's own foreground, not the accent:
/// on this machine the accent is red.
fn paint(up: bool) -> [u8; 3] {
    if !up {
        return [0x8E, 0x8E, 0x8E];
    }
    glyph_rgb()
}

#[cfg(windows)]
fn glyph_rgb() -> [u8; 3] {
    super::wincolor::neutral()
}

#[cfg(target_os = "macos")]
fn glyph_rgb() -> [u8; 3] {
    // A template image. AppKit tints the black silhouette.
    [0, 0, 0]
}

#[cfg(target_os = "linux")]
fn glyph_rgb() -> [u8; 3] {
    linux_rgb()
}

#[cfg(target_os = "linux")]
fn linux_rgb() -> [u8; 3] {
    static TICKS: AtomicU32 = AtomicU32::new(0);
    static CACHED: AtomicU32 = AtomicU32::new(u32::MAX);
    let n = TICKS.fetch_add(1, Ordering::Relaxed);
    let cached = CACHED.load(Ordering::Relaxed);
    if cached != u32::MAX && n % 120 != 0 {
        return [(cached >> 16) as u8, (cached >> 8) as u8, cached as u8];
    }
    // Unknown scheme: a light palm, so a dark panel does not hide it.
    let dark = panel_dark().unwrap_or(true);
    let rgb = if dark { [0xE6, 0xE6, 0xE6] } else { [0x20, 0x20, 0x20] };
    CACHED.store(pack(rgb), Ordering::Relaxed);
    rgb
}

#[cfg(target_os = "linux")]
fn panel_dark() -> Option<bool> {
    if let Some(text) = gsettings("color-scheme") {
        if text.contains("prefer-dark") {
            return Some(true);
        }
        if text.contains("prefer-light") {
            return Some(false);
        }
    }
    gsettings("gtk-theme").map(|text| text.to_ascii_lowercase().contains("dark"))
}

#[cfg(target_os = "linux")]
fn gsettings(key: &str) -> Option<String> {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// The icon belongs to tray-icon's hidden window (`tray_icon_app`).
/// `Shell_NotifyIconW` + `NIF_INFO` is what Windows 10/11 draws as a toast,
/// and that toast always keeps its header. This posts nothing to the shell.
/// The notice is a borderless topmost tool window anchored to the icon.
#[cfg(windows)]
fn win_balloon(line: &str, up: bool) -> bool {
    let Some((hwnd, uid)) = find_tray() else {
        warn_balloon("icon not found");
        return false;
    };
    let palm = balloon_palm(up);
    if palm == 0 {
        return false;
    }
    let Some(anchor) = icon_rect(hwnd, uid) else {
        warn_balloon("icon rect failed");
        return false;
    };
    show_tip(hwnd, line, palm, anchor)
}

#[cfg(windows)]
fn warn_balloon(why: &str) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::warn!("tray balloon: {why}");
}

#[cfg(windows)]
fn find_tray() -> Option<(isize, u32)> {
    let mut hit = Hit { hwnd: 0, uid: 0 };
    unsafe { EnumWindows(hunt, &mut hit as *mut Hit as isize) };
    if hit.hwnd == 0 {
        None
    } else {
        Some((hit.hwnd, hit.uid))
    }
}

#[cfg(windows)]
struct Hit {
    hwnd: isize,
    uid: u32,
}

#[cfg(windows)]
unsafe extern "system" fn hunt(hwnd: isize, param: isize) -> i32 {
    let hit = &mut *(param as *mut Hit);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid != GetCurrentProcessId() {
        return 1;
    }
    let mut class = [0u16; 64];
    let n = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    if n <= 0 {
        return 1;
    }
    let name = String::from_utf16_lossy(&class[..n as usize]);
    if name != "tray_icon_app" {
        return 1;
    }
    if let Some(uid) = probe_uid(hwnd) {
        hit.hwnd = hwnd;
        hit.uid = uid;
        return 0;
    }
    1
}

#[cfg(windows)]
fn probe_uid(hwnd: isize) -> Option<u32> {
    for uid in 1..16u32 {
        let id = NotifyId {
            cb_size: std::mem::size_of::<NotifyId>() as u32,
            hwnd,
            uid,
            guid: Guid { data1: 0, data2: 0, data3: 0, data4: [0; 8] },
        };
        let mut rect = WinRect { left: 0, top: 0, right: 0, bottom: 0 };
        if unsafe { Shell_NotifyIconGetRect(&id, &mut rect) } == 0 {
            return Some(uid);
        }
    }
    None
}

#[cfg(windows)]
fn icon_rect(hwnd: isize, uid: u32) -> Option<WinRect> {
    let id = NotifyId {
        cb_size: std::mem::size_of::<NotifyId>() as u32,
        hwnd,
        uid,
        guid: Guid { data1: 0, data2: 0, data3: 0, data4: [0; 8] },
    };
    let mut rect = WinRect { left: 0, top: 0, right: 0, bottom: 0 };
    if unsafe { Shell_NotifyIconGetRect(&id, &mut rect) } == 0 {
        Some(rect)
    } else {
        None
    }
}

/// One palm per link state. The tip window draws this handle.
/// It lives until the process exits: the window must not destroy it.
#[cfg(windows)]
fn balloon_palm(up: bool) -> isize {
    use std::sync::atomic::AtomicIsize;
    static PALMS: [AtomicIsize; 2] = [AtomicIsize::new(0), AtomicIsize::new(0)];
    let slot = &PALMS[usize::from(up)];
    let existing = slot.load(Ordering::SeqCst);
    if existing != 0 {
        return existing;
    }
    let handle = palm_icon(&balloon_rgba(up));
    if handle == 0 {
        warn_balloon("palm icon failed");
        return 0;
    }
    slot.store(handle, Ordering::SeqCst);
    handle
}

/// The baked palm keeps its greens and trunk when the link comes up.
/// Down, the same pixels with the color taken out.
#[cfg(windows)]
fn balloon_rgba(up: bool) -> Vec<u8> {
    if up {
        return PALM.to_vec();
    }
    let mut out = vec![0u8; PALM.len()];
    for (dst, src) in out.chunks_exact_mut(4).zip(PALM.chunks_exact(4)) {
        let y = (u32::from(src[0]) * 54 + u32::from(src[1]) * 183 + u32::from(src[2]) * 19) / 256;
        let y = y as u8;
        dst[0] = y;
        dst[1] = y;
        dst[2] = y;
        dst[3] = src[3];
    }
    out
}

#[cfg(windows)]
fn palm_icon(rgba: &[u8]) -> isize {
    let pixels = rgba.len() / 4;
    let mut bgra = vec![0u8; rgba.len()];
    let mut mask = vec![0u8; pixels];
    for i in 0..pixels {
        let o = i * 4;
        bgra[o] = rgba[o + 2];
        bgra[o + 1] = rgba[o + 1];
        bgra[o + 2] = rgba[o];
        bgra[o + 3] = rgba[o + 3];
        mask[i] = rgba[o + 3].wrapping_sub(u8::MAX);
    }
    unsafe { CreateIcon(0, SIDE as i32, SIDE as i32, 1, 32, mask.as_ptr(), bgra.as_ptr()) }
}

#[cfg(windows)]
#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[cfg(windows)]
#[repr(C)]
struct NotifyId {
    cb_size: u32,
    hwnd: isize,
    uid: u32,
    guid: Guid,
}

#[cfg(all(windows, target_arch = "x86_64"))]
const _: () = assert!(std::mem::size_of::<NotifyId>() == 40);

#[cfg(windows)]
#[repr(C)]
struct WinRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[cfg(windows)]
#[link(name = "shell32")]
extern "system" {
    fn Shell_NotifyIconGetRect(id: *const NotifyId, rect: *mut WinRect) -> i32;
}

#[cfg(windows)]
#[link(name = "user32")]
extern "system" {
    fn EnumWindows(cb: unsafe extern "system" fn(isize, isize) -> i32, param: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn GetClassNameW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn CreateIcon(
        instance: isize,
        width: i32,
        height: i32,
        planes: u32,
        bits_per_pixel: u32,
        and_bits: *const u8,
        xor_bits: *const u8,
    ) -> isize;
    fn RegisterClassExW(class: *const WndClass) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class: *const u16,
        window: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: isize,
        menu: isize,
        instance: isize,
        param: *mut core::ffi::c_void,
    ) -> isize;
    fn DestroyWindow(hwnd: isize) -> i32;
    fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, w: i32, h: i32, flags: u32) -> i32;
    fn SetWindowRgn(hwnd: isize, region: isize, redraw: i32) -> i32;
    fn DefWindowProcW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn BeginPaint(hwnd: isize, paint: *mut Paint) -> isize;
    fn EndPaint(hwnd: isize, paint: *const Paint) -> i32;
    fn FillRect(hdc: isize, rect: *const WinRect, brush: isize) -> i32;
    fn DrawTextW(hdc: isize, text: *const u16, n: i32, rect: *mut WinRect, format: u32) -> i32;
    fn DrawIconEx(
        hdc: isize,
        x: i32,
        y: i32,
        icon: isize,
        w: i32,
        h: i32,
        step: u32,
        brush: isize,
        flags: u32,
    ) -> i32;
    fn GetClientRect(hwnd: isize, rect: *mut WinRect) -> i32;
    fn LoadCursorW(instance: isize, name: *const u16) -> isize;
    fn SetTimer(hwnd: isize, id: usize, elapse: u32, proc: *const core::ffi::c_void) -> usize;
    fn GetDC(hwnd: isize) -> isize;
    fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
    fn MonitorFromRect(rect: *const WinRect, flags: u32) -> isize;
    fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn GetDpiForWindow(hwnd: isize) -> u32;
}

#[cfg(windows)]
#[link(name = "gdi32")]
extern "system" {
    fn CreateFontW(
        height: i32,
        width: i32,
        escapement: i32,
        orientation: i32,
        weight: i32,
        italic: u32,
        underline: u32,
        strike: u32,
        charset: u32,
        out_precision: u32,
        clip_precision: u32,
        quality: u32,
        pitch: u32,
        face: *const u16,
    ) -> isize;
    fn CreateSolidBrush(color: u32) -> isize;
    fn CreateRoundRectRgn(left: i32, top: i32, right: i32, bottom: i32, w: i32, h: i32) -> isize;
    fn SelectObject(hdc: isize, object: isize) -> isize;
    fn DeleteObject(object: isize) -> i32;
    fn GetStockObject(index: i32) -> isize;
    fn SetBkMode(hdc: isize, mode: i32) -> i32;
    fn SetTextColor(hdc: isize, color: u32) -> u32;
    fn GetTextExtentPoint32W(hdc: isize, text: *const u16, n: i32, size: *mut WinSize) -> i32;
}

#[cfg(windows)]
#[link(name = "shcore")]
extern "system" {
    fn GetDpiForMonitor(monitor: isize, kind: i32, x: *mut u32, y: *mut u32) -> i32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcessId() -> u32;
    fn GetModuleHandleW(name: *const u16) -> isize;
    fn GetLastError() -> u32;
}

#[cfg(windows)]
#[repr(C)]
struct WndClass {
    cb_size: u32,
    style: u32,
    wnd_proc: unsafe extern "system" fn(isize, u32, usize, isize) -> isize,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    instance: isize,
    icon: isize,
    cursor: isize,
    brush: isize,
    menu_name: *const u16,
    class_name: *const u16,
    icon_sm: isize,
}

#[cfg(windows)]
#[repr(C)]
struct Paint {
    hdc: isize,
    erase: i32,
    paint: WinRect,
    restore: i32,
    inc_update: i32,
    reserved: [u8; 32],
}

#[cfg(windows)]
#[repr(C)]
struct MonitorInfo {
    cb_size: u32,
    monitor: WinRect,
    work: WinRect,
    flags: u32,
}

#[cfg(windows)]
#[repr(C)]
struct WinSize {
    cx: i32,
    cy: i32,
}

#[cfg(all(windows, target_arch = "x86_64"))]
const _: () = assert!(std::mem::size_of::<WndClass>() == 80);
#[cfg(all(windows, target_arch = "x86_64"))]
const _: () = assert!(std::mem::size_of::<Paint>() == 72);
#[cfg(all(windows, target_arch = "x86_64"))]
const _: () = assert!(std::mem::size_of::<MonitorInfo>() == 40);

#[cfg(windows)]
const WS_POPUP: u32 = 0x8000_0000;
#[cfg(windows)]
const WS_EX_TOPMOST: u32 = 0x0000_0008;
#[cfg(windows)]
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
#[cfg(windows)]
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
#[cfg(windows)]
const SW_SHOWNOACTIVATE: i32 = 4;
#[cfg(windows)]
const SWP_NOSIZE: u32 = 0x0001;
#[cfg(windows)]
const SWP_NOMOVE: u32 = 0x0002;
#[cfg(windows)]
const SWP_NOACTIVATE: u32 = 0x0010;
#[cfg(windows)]
const SWP_SHOWWINDOW: u32 = 0x0040;
#[cfg(windows)]
const HWND_TOPMOST: isize = -1;
#[cfg(windows)]
const CS_VREDRAW: u32 = 0x0001;
#[cfg(windows)]
const CS_HREDRAW: u32 = 0x0002;
#[cfg(windows)]
const CS_DROPSHADOW: u32 = 0x0002_0000;
#[cfg(windows)]
const WM_DESTROY: u32 = 0x0002;
#[cfg(windows)]
const WM_ERASEBKGND: u32 = 0x0014;
#[cfg(windows)]
const WM_MOUSEACTIVATE: u32 = 0x0021;
#[cfg(windows)]
const WM_PAINT: u32 = 0x000F;
#[cfg(windows)]
const WM_TIMER: u32 = 0x0113;
#[cfg(windows)]
const WM_LBUTTONDOWN: u32 = 0x0201;
#[cfg(windows)]
const WM_RBUTTONDOWN: u32 = 0x0204;
#[cfg(windows)]
const WM_MBUTTONDOWN: u32 = 0x0207;
#[cfg(windows)]
const MA_NOACTIVATE: isize = 3;
#[cfg(windows)]
const IDC_ARROW: isize = 32512;
#[cfg(windows)]
const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
#[cfg(windows)]
const DT_VCENTER: u32 = 0x0004;
#[cfg(windows)]
const DT_SINGLELINE: u32 = 0x0020;
#[cfg(windows)]
const DT_NOPREFIX: u32 = 0x0800;
#[cfg(windows)]
const DT_END_ELLIPSIS: u32 = 0x8000;
#[cfg(windows)]
const BK_TRANSPARENT: i32 = 1;
#[cfg(windows)]
const DI_NORMAL: u32 = 0x0003;
#[cfg(windows)]
const MONITOR_DEFAULTTONEAREST: u32 = 2;
#[cfg(windows)]
const DEFAULT_GUI_FONT: i32 = 17;
#[cfg(windows)]
const CLEARTYPE_QUALITY: u32 = 5;

#[cfg(windows)]
struct Tip {
    hwnd: isize,
    born: Instant,
    paint: TipPaint,
}

#[cfg(windows)]
#[derive(Clone)]
struct TipPaint {
    line: Vec<u16>,
    palm: isize,
    bg: u32,
    fg: u32,
    edge: u32,
    pad: i32,
    icon: i32,
    gap: i32,
    dpi: u32,
}

#[cfg(windows)]
thread_local! {
    static TIP: RefCell<Option<Tip>> = RefCell::new(None);
}

#[cfg(windows)]
fn show_tip(tray: isize, line: &str, palm: isize, anchor: WinRect) -> bool {
    if !ensure_class() {
        return false;
    }
    close_tip();
    let dpi = tip_dpi(tray, &anchor);
    let (bg, fg, edge) = card_colors();
    let pad = px(12, dpi);
    let icon = px(32, dpi);
    let gap = px(10, dpi);
    let wide = utf16(line);
    let (text_w, text_h) = text_px(&wide, dpi);
    let text_w = text_w.min(px(480, dpi)).max(px(40, dpi));
    let width = pad + icon + gap + text_w + pad;
    let height = pad + icon.max(text_h) + pad;
    if width < 8 || height < 8 {
        warn_balloon("tip size failed");
        return false;
    }
    let (x, y) = place(&anchor, width, height, dpi);
    let paint = TipPaint {
        line: wide,
        palm,
        bg: cref(bg),
        fg: cref(fg),
        edge: cref(edge),
        pad,
        icon,
        gap,
        dpi,
    };
    TIP.with(|slot| {
        *slot.borrow_mut() = Some(Tip { hwnd: 0, born: Instant::now(), paint });
    });
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name().as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            x,
            y,
            width,
            height,
            0,
            0,
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null_mut(),
        )
    };
    if hwnd == 0 {
        TIP.with(|slot| *slot.borrow_mut() = None);
        warn_balloon("window failed");
        return false;
    }
    TIP.with(|slot| {
        if let Some(tip) = slot.borrow_mut().as_mut() {
            tip.hwnd = hwnd;
            tip.born = Instant::now();
        }
    });
    let rad = px(8, dpi).max(4);
    unsafe {
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, rad, rad);
        if region != 0 {
            SetWindowRgn(hwnd, region, 0);
        }
        SetTimer(hwnd, 1, 2000, std::ptr::null());
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
    true
}

#[cfg(windows)]
fn expire_tip() {
    let due = TIP.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|tip| tip.hwnd != 0 && tip.born.elapsed() >= Duration::from_millis(2000))
            .unwrap_or(false)
    });
    if due {
        close_tip();
    }
}

#[cfg(windows)]
fn close_tip() {
    let hwnd = TIP.with(|slot| slot.borrow().as_ref().map(|tip| tip.hwnd).unwrap_or(0));
    if hwnd != 0 {
        unsafe { DestroyWindow(hwnd) };
    }
}

#[cfg(windows)]
fn forget_tip(hwnd: isize) {
    TIP.with(|slot| {
        let matches = slot.borrow().as_ref().map(|tip| tip.hwnd == hwnd).unwrap_or(false);
        if matches {
            *slot.borrow_mut() = None;
        }
    });
}

#[cfg(windows)]
fn ensure_class() -> bool {
    static READY: AtomicBool = AtomicBool::new(false);
    if READY.load(Ordering::SeqCst) {
        return true;
    }
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class = WndClass {
        cb_size: std::mem::size_of::<WndClass>() as u32,
        style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
        wnd_proc: tip_proc,
        cb_cls_extra: 0,
        cb_wnd_extra: 0,
        instance,
        icon: 0,
        cursor: unsafe { LoadCursorW(0, IDC_ARROW as *const u16) },
        brush: 0,
        menu_name: std::ptr::null(),
        class_name: class_name().as_ptr(),
        icon_sm: 0,
    };
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
        warn_balloon("class failed");
        return false;
    }
    READY.store(true, Ordering::SeqCst);
    true
}

#[cfg(windows)]
fn class_name() -> &'static [u16] {
    use std::sync::OnceLock;
    static NAME: OnceLock<Vec<u16>> = OnceLock::new();
    NAME.get_or_init(|| utf16("m1r4g3_tray_tip")).as_slice()
}

#[cfg(windows)]
unsafe extern "system" fn tip_proc(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize {
    match msg {
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE,
        WM_TIMER | WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            DestroyWindow(hwnd);
            0
        }
        WM_PAINT => {
            paint_tip(hwnd);
            0
        }
        WM_DESTROY => {
            forget_tip(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(windows)]
fn paint_tip(hwnd: isize) {
    let paint = TIP.with(|slot| slot.borrow().as_ref().map(|tip| tip.paint.clone()));
    let Some(paint) = paint else {
        return;
    };
    let mut info = Paint {
        hdc: 0,
        erase: 0,
        paint: WinRect { left: 0, top: 0, right: 0, bottom: 0 },
        restore: 0,
        inc_update: 0,
        reserved: [0; 32],
    };
    let hdc = unsafe { BeginPaint(hwnd, &mut info) };
    if hdc == 0 {
        return;
    }
    let mut client = WinRect { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetClientRect(hwnd, &mut client) };
    let edge = Brush(unsafe { CreateSolidBrush(paint.edge) });
    let bg = Brush(unsafe { CreateSolidBrush(paint.bg) });
    unsafe { FillRect(hdc, &client, edge.0) };
    let inner = WinRect {
        left: client.left + 1,
        top: client.top + 1,
        right: client.right - 1,
        bottom: client.bottom - 1,
    };
    if inner.right > inner.left && inner.bottom > inner.top {
        unsafe { FillRect(hdc, &inner, bg.0) };
    }
    let icon_y = client.top + (client.bottom - client.top - paint.icon) / 2;
    if paint.palm != 0 {
        unsafe {
            DrawIconEx(
                hdc,
                client.left + paint.pad,
                icon_y,
                paint.palm,
                paint.icon,
                paint.icon,
                0,
                0,
                DI_NORMAL,
            );
        }
    }
    let font = font_for(paint.dpi);
    let old = unsafe { SelectObject(hdc, font.handle) };
    unsafe {
        SetBkMode(hdc, BK_TRANSPARENT);
        SetTextColor(hdc, paint.fg);
    }
    let mut text = WinRect {
        left: client.left + paint.pad + paint.icon + paint.gap,
        top: client.top,
        right: client.right - paint.pad,
        bottom: client.bottom,
    };
    unsafe {
        DrawTextW(
            hdc,
            paint.line.as_ptr(),
            -1,
            &mut text,
            DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        SelectObject(hdc, old);
        EndPaint(hwnd, &info);
    }
}

#[cfg(windows)]
fn place(anchor: &WinRect, width: i32, height: i32, dpi: u32) -> (i32, i32) {
    let gap = px(8, dpi);
    let work = work_area(anchor);
    let mid_x = work.left + (work.right - work.left) / 2;
    let mut x = if anchor.left >= mid_x { anchor.right - width } else { anchor.left };
    let mut y = anchor.top - gap - height;
    let below = anchor.bottom + gap;
    if y < work.top && below + height <= work.bottom {
        y = below;
    } else if y < work.top {
        y = anchor.top + (anchor.bottom - anchor.top - height) / 2;
        x = if anchor.left >= mid_x {
            anchor.left - gap - width
        } else {
            anchor.right + gap
        };
    }
    if x + width > work.right {
        x = work.right - width;
    }
    if x < work.left {
        x = work.left;
    }
    if y + height > work.bottom {
        y = work.bottom - height;
    }
    if y < work.top {
        y = work.top;
    }
    (x, y)
}

#[cfg(windows)]
fn work_area(anchor: &WinRect) -> WinRect {
    unsafe {
        let monitor = MonitorFromRect(anchor, MONITOR_DEFAULTTONEAREST);
        let mut info = MonitorInfo {
            cb_size: std::mem::size_of::<MonitorInfo>() as u32,
            monitor: WinRect { left: 0, top: 0, right: 0, bottom: 0 },
            work: WinRect { left: 0, top: 0, right: 0, bottom: 0 },
            flags: 0,
        };
        if monitor != 0 && GetMonitorInfoW(monitor, &mut info) != 0 {
            return info.work;
        }
        let left = GetSystemMetrics(76);
        let top = GetSystemMetrics(77);
        WinRect {
            left,
            top,
            right: left + GetSystemMetrics(78),
            bottom: top + GetSystemMetrics(79),
        }
    }
}

#[cfg(windows)]
fn tip_dpi(tray: isize, anchor: &WinRect) -> u32 {
    unsafe {
        let monitor = MonitorFromRect(anchor, MONITOR_DEFAULTTONEAREST);
        if monitor != 0 {
            let mut x = 0u32;
            let mut y = 0u32;
            if GetDpiForMonitor(monitor, 0, &mut x, &mut y) == 0 && x >= 96 {
                return x;
            }
        }
        let dpi = GetDpiForWindow(tray);
        if dpi >= 96 { dpi } else { 96 }
    }
}

#[cfg(windows)]
fn text_px(line: &[u16], dpi: u32) -> (i32, i32) {
    let fallback = (px(180, dpi), px(20, dpi));
    unsafe {
        let dc = GetDC(0);
        if dc == 0 {
            return fallback;
        }
        let font = font_for(dpi);
        let old = SelectObject(dc, font.handle);
        let mut size = WinSize { cx: 0, cy: 0 };
        let n = line.iter().take_while(|unit| **unit != 0).count() as i32;
        if n > 0 {
            GetTextExtentPoint32W(dc, line.as_ptr(), n, &mut size);
        }
        SelectObject(dc, old);
        drop(font);
        ReleaseDC(0, dc);
        if size.cx <= 0 || size.cy <= 0 { fallback } else { (size.cx, size.cy) }
    }
}

#[cfg(windows)]
fn font_for(dpi: u32) -> GdiFont {
    let face = utf16("Segoe UI");
    let handle = unsafe {
        CreateFontW(
            -px(15, dpi),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            1,
            0,
            0,
            CLEARTYPE_QUALITY,
            0,
            face.as_ptr(),
        )
    };
    if handle != 0 {
        GdiFont { handle, owned: true }
    } else {
        GdiFont { handle: unsafe { GetStockObject(DEFAULT_GUI_FONT) }, owned: false }
    }
}

#[cfg(windows)]
struct GdiFont {
    handle: isize,
    owned: bool,
}

#[cfg(windows)]
impl Drop for GdiFont {
    fn drop(&mut self) {
        if self.owned && self.handle != 0 {
            unsafe { DeleteObject(self.handle) };
            self.handle = 0;
        }
    }
}

#[cfg(windows)]
struct Brush(isize);

#[cfg(windows)]
impl Drop for Brush {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { DeleteObject(self.0) };
            self.0 = 0;
        }
    }
}

#[cfg(windows)]
fn card_colors() -> ([u8; 3], [u8; 3], [u8; 3]) {
    if super::wincolor::system_dark() {
        ([0x2C, 0x2C, 0x2C], [0xF3, 0xF3, 0xF3], [0x3F, 0x3F, 0x3F])
    } else {
        ([0xF7, 0xF7, 0xF7], [0x1A, 0x1A, 0x1A], [0xD6, 0xD6, 0xD6])
    }
}

#[cfg(windows)]
fn cref(rgb: [u8; 3]) -> u32 {
    u32::from(rgb[0]) | u32::from(rgb[1]) << 8 | u32::from(rgb[2]) << 16
}

#[cfg(windows)]
fn px(n: i32, dpi: u32) -> i32 {
    n * dpi as i32 / 96
}

#[cfg(windows)]
fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
