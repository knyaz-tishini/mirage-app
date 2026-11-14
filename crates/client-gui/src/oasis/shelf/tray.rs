//! The tray icon this process owns.
//! uzor's tray builds the icon and the labels once, so a language change
//! and a theme change never reach it. This one is created on the first
//! UI tick and updates both.
//!
//! The menu is one action (подключиться / отключиться) and quit.
//! The hover tip is the app name and version. The palm is gray when the
//! link is down and the colored palm when it is up. The system toast
//! header copies this tray icon, so the header matches it.
//! A state change posts the system balloon Amnezia posts from
//! `QSystemTrayIcon::showMessage`: one line, the colored palm when the
//! link comes up and the same palm in gray when it goes down.
//! "Disconnected" is sent only after a real link.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

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
/// The watcher is one thread for the life of the process.
static WATCHER: Once = Once::new();

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
    } else {
        FAILED.store(true, Ordering::SeqCst);
    }
}

fn open(up: bool) -> Option<Live> {
    #[cfg(windows)]
    super::wincolor::allow_dark_menus();
    let icon = match Icon::from_rgba(tray_rgba(up), SIDE, SIDE) {
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
        .with_icon_as_template(false)
        .build();
    let tray = match tray {
        Ok(tray) => tray,
        Err(err) => {
            tracing::warn!("tray shelf: {err}");
            return None;
        }
    };
    tracing::info!("tray shelf");
    WATCHER.call_once(spawn_watcher);
    Some(Live {
        tray,
        link,
        quit,
        lang: crate::text::current() as u8,
        ink: tray_ink(up),
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
    // The toast header copies the tray icon, so the picture changes
    // before the balloon is posted.
    let ink = tray_ink(up);
    if live.ink != ink {
        live.ink = ink;
        if let Ok(icon) = Icon::from_rgba(tray_rgba(up), SIDE, SIDE) {
            let _ = live.tray.set_icon(Some(icon));
        }
    }
    announce(live, up);
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

/// The tray calls the window back.
///
/// The click does not wait for the picture: the window is shown right here,
/// from the tray's own thread, so a window that was closed to the tray comes
/// back even while nothing is on the screen. The word goes to the picture
/// too, so the scene runs at its usual rate again.
fn spawn_watcher() {
    let spawned = std::thread::Builder::new()
        .name("mirage-tray-watch".into())
        .spawn(|| {
            let receiver = TrayIconEvent::receiver();
            loop {
                match receiver.try_recv() {
                    Ok(TrayIconEvent::Click {
                        button,
                        button_state,
                        ..
                    }) => {
                        if button == MouseButton::Left && button_state == MouseButtonState::Up {
                            tracing::info!("tray shelf: the window is called back");
                            crate::oasis::windowctl::show();
                            post(Cmd::Show);
                        }
                    }
                    Ok(_) => {}
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(40)),
                }
            }
        });
    if spawned.is_err() {
        tracing::warn!("tray shelf: the watcher did not start");
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

/// Tray picture. Off is one gray. On is the baked palm, greens and trunk.
/// The accent is not used. A template image would throw the color away,
/// so the connected palm is a normal icon on every platform.
fn tray_rgba(up: bool) -> Vec<u8> {
    if up {
        PALM.to_vec()
    } else {
        palm([0x8E, 0x8E, 0x8E])
    }
}

fn tray_ink(up: bool) -> u32 {
    u32::from(up)
}

/// `QSystemTrayIcon::showMessage` on Windows is `Shell_NotifyIconW` with
/// `NIF_INFO`. The icon already belongs to tray-icon's hidden window
/// (`tray_icon_app`); this finds that window in this process and posts
/// the balloon on it. Two seconds, same as Amnezia.
#[cfg(windows)]
fn win_balloon(line: &str, up: bool) -> bool {
    let Some((hwnd, uid)) = find_tray() else {
        warn_balloon("icon not found");
        return false;
    };
    // NIIF_INFO is the system "i". NIIF_USER draws hBalloonIcon instead.
    let palm = balloon_palm(up);
    let info_flags = if palm == 0 { NIIF_INFO } else { NIIF_USER | NIIF_LARGE_ICON };
    let mut nid = NotifyIcon {
        cb_size: std::mem::size_of::<NotifyIcon>() as u32,
        hwnd,
        uid,
        flags: NIF_INFO | NIF_SHOWTIP,
        callback: 0,
        icon: 0,
        tip: [0; 128],
        state: 0,
        state_mask: 0,
        info: [0; 256],
        timeout: 2000,
        title: [0; 64],
        info_flags,
        guid: Guid { data1: 0, data2: 0, data3: 0, data4: [0; 8] },
        balloon_icon: palm,
    };
    write_wide(&mut nid.info, line);
    let ok = unsafe { Shell_NotifyIconW(NIM_MODIFY, &mut nid) };
    if ok == 0 {
        warn_balloon("notify failed");
        return false;
    }
    true
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
fn write_wide(dst: &mut [u16], text: &str) {
    for (i, unit) in text.encode_utf16().enumerate() {
        if i + 1 >= dst.len() {
            break;
        }
        dst[i] = unit;
    }
}

#[cfg(windows)]
const NIF_INFO: u32 = 0x10;
#[cfg(windows)]
const NIF_SHOWTIP: u32 = 0x80;
#[cfg(windows)]
const NIIF_INFO: u32 = 0x1;
#[cfg(windows)]
const NIIF_USER: u32 = 0x4;
#[cfg(windows)]
const NIIF_LARGE_ICON: u32 = 0x20;
#[cfg(windows)]
const NIM_MODIFY: u32 = 0x1;

/// One palm per link state. The shell keeps the handle for the balloon,
/// so these live until the process exits.
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

/// x64 `NOTIFYICONDATAW`. `cbSize` is `size_of` this struct, which is the
/// Vista layout tray-icon already registers the icon with.
#[cfg(windows)]
#[repr(C)]
struct NotifyIcon {
    cb_size: u32,
    hwnd: isize,
    uid: u32,
    flags: u32,
    callback: u32,
    icon: isize,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    timeout: u32,
    title: [u16; 64],
    info_flags: u32,
    guid: Guid,
    balloon_icon: isize,
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
const _: () = assert!(std::mem::size_of::<NotifyIcon>() == 976);
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
    fn Shell_NotifyIconW(message: u32, data: *mut NotifyIcon) -> i32;
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
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcessId() -> u32;
}
