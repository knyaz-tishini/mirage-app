//! UI copy. Russian and English. The language is the OS interface language
//! until the person picks one; the pick is a `lang` file beside the exe
//! (`ru` or `en`), same place as `client-mode`.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Lang {
    Ru = 0,
    En = 1,
}

const UNSET: u8 = 255;
static CUR: AtomicU8 = AtomicU8::new(UNSET);

pub fn current() -> Lang {
    match CUR.load(Ordering::Relaxed) {
        0 => Lang::Ru,
        1 => Lang::En,
        _ => {
            let lang = stored().unwrap_or_else(system);
            CUR.store(lang as u8, Ordering::Relaxed);
            lang
        }
    }
}

/// The name shown on the language row.
pub fn current_name() -> &'static str {
    match current() {
        Lang::Ru => "русский",
        Lang::En => "english",
    }
}

/// Flip ru ↔ en and remember it beside the exe.
pub fn cycle() {
    let next = match current() {
        Lang::Ru => Lang::En,
        Lang::En => Lang::Ru,
    };
    CUR.store(next as u8, Ordering::Relaxed);
    let _ = write_stored(next);
}

pub fn tr(id: &'static str) -> &'static str {
    let (ru, en) = row(id);
    match current() {
        Lang::Ru => ru,
        Lang::En => en,
    }
}

fn row(id: &'static str) -> (&'static str, &'static str) {
    match id {
        "mode.mesh" => ("меш", "mesh"),
        "mode.own" => ("свой сервер", "own server"),

        "h.connections" => ("подключения", "connections"),
        "h.settings" => ("настройки", "settings"),

        "t.cabinet" => ("кабинет", "cabinet"),
        "t.bound" => ("флот: привязан", "fleet: bound"),
        "t.unbound" => ("флот: не привязан", "fleet: not bound"),
        "t.vpn" => ("vpn — один бокс", "vpn — one box"),
        "t.onion" => ("onion — цепочка", "onion — a chain"),
        "t.none" => ("ничего не выбрано", "nothing picked"),
        "t.empty" => ("пока пусто", "nothing yet"),

        "nav.servers" => ("серверы", "servers"),
        "nav.device" => ("устройство", "device"),
        "nav.cabinet" => ("кабинет", "cabinet"),

        "f.host" => ("хост", "host"),
        "f.cabinet" => ("кабинет", "cabinet"),
        "f.user" => ("логин", "user"),
        "f.pass" => ("пароль", "password"),
        "f.build" => ("режим", "mode"),
        "f.kit" => ("комплект", "kit"),
        "f.binary" => ("бинарь", "binary"),
        "f.sshkey" => ("ключ", "key"),
        "f.keysrc" => ("ключ бокса", "box key"),
        "f.mwg" => ("порт vg", "vg port"),
        "f.kvik" => ("порт kvik", "kvik port"),
        "f.stack" => ("стек", "stack"),
        "f.port" => ("порт", "port"),
        "f.link" => ("ссылка", "link"),
        "f.paste" => ("вставить", "paste"),
        "f.guest" => ("гость", "guest"),
        "f.name" => ("имя", "name"),
        "f.kill" => ("киллсвитч", "kill switch"),
        "f.carry" => ("как несём трафик", "how traffic leaves"),
        "f.dns" => ("dns", "dns"),
        "f.resolver" => ("свой резолвер", "own resolver"),
        "f.except" => ("исключения", "exceptions"),
        "f.dns_list" => ("dns через запятую", "dns, comma separated"),
        "f.routes" => ("маршруты", "routes"),
        "f.site" => ("сайт", "site"),
        "f.domain" => ("домен", "domain"),
        "f.app" => ("приложение", "app"),
        "f.process" => ("процесс", "process"),
        "f.lang" => ("язык", "language"),
        "f.style" => ("стиль", "style"),
        "theme.light" => ("светлая", "light"),
        "theme.dark" => ("тёмная", "dark"),

        "b.install" => ("установить", "install"),
        "b.roll" => ("накатить", "roll"),
        "b.forget" => ("забыть", "forget"),
        "b.page_new" => ("новый сервер", "new server"),
        "b.page_known" => ("имеющиеся", "existing"),
        "b.assemble" => ("собрать", "assemble"),
        "b.assembling" => ("собираю…", "assembling…"),
        "b.up" => ("выше", "up"),
        "b.down" => ("ниже", "down"),
        "role.relay" => ("релей", "relay"),
        "role.exit" => ("терминатор", "terminator"),
        "b.installing" => ("ставится…", "installing…"),
        "b.config" => ("конфиг", "config"),
        "b.fetching" => ("забираю…", "fetching…"),
        "b.remove" => ("снять", "remove"),
        "b.removing" => ("снимается…", "removing…"),
        "b.export" => ("экспорт", "export"),
        "b.copy" => ("копировать", "copy"),
        "b.scan" => ("сканер", "scan"),
        "b.file" => ("файл", "file"),
        "b.clip" => ("буфер", "clipboard"),
        "b.invite_conn" => ("подключение", "connection"),
        "b.invite_full" => ("полный", "full"),
        "b.my_key" => ("мой ключ", "my key"),
        "b.unlink" => ("отвязать", "unlink"),
        "shelf.connect" => ("подключиться", "connect"),
        "shelf.disconnect" => ("отключиться", "disconnect"),
        "shelf.quit" => ("выйти", "quit"),
        // One line on the tray balloon. No separate title above it.
        "shelf.state.on" => ("m1r4g3 подключен", "m1r4g3 connected"),
        "shelf.state.off" => ("m1r4g3 отключен", "m1r4g3 disconnected"),

        "b.on" => ("вкл", "on"),
        "b.off" => ("выкл", "off"),
        "b.revoke" => ("отозвать", "revoke"),
        "b.drop" => ("убрать", "remove"),

        "c.auto" => ("авто", "auto"),
        "c.soft" => ("мягкий", "soft"),
        "c.strict" => ("жёсткий", "strict"),
        "c.bypass" => ("все кроме", "all except"),
        "c.only" => ("только эти", "only these"),
        "c.socks" => ("локальный socks5", "local socks5"),
        "c.tun" => ("весь трафик в туннель", "whole machine in tunnel"),
        "c.build_ready" => ("готовый стек", "ready stack"),
        "c.build_own" => ("свой бинарь", "own binary"),
        "c.key_own" => ("свой ключ", "own key"),
        "c.key_file" => ("ключ из файла", "key from file"),

        "kit.qm" => ("quic + m1r4g3", "quic + m1r4g3"),
        "kit.qm.d" => (
            "quic UDP 443, m1r4g3 TCP 443 80 8080 8443",
            "quic UDP 443, m1r4g3 TCP 443 80 8080 8443",
        ),
        "kit.quic" => ("quic", "quic"),
        "kit.quic.d" => ("UDP 443", "UDP 443"),
        "kit.all" => ("все протоколы", "all protocols"),
        "kit.all.d" => ("дефолтные порты, камуфляж none", "default ports, camouflage none"),

        "bind.cabinet" => ("кабинет", "cabinet"),
        "bind.unlink" => ("отвязать", "unlink"),
        "bind.confirm" => ("подтвердить", "confirm"),
        "bind.go" => ("привязать", "bind"),
        "bind.ph" => ("код или ссылка /c/", "code or /c/ link"),
        "bind.no_client" => ("привязка: нет клиента", "bind: no client"),
        "bind.fail" => ("привязка", "bind"),
        "bind.not_started" => ("привязка: не запустилась", "bind: did not start"),
        "bind.ok_stale" => ("привязано, процесс не перезапустился", "bound, the process did not restart"),
        "bind.ok" => ("привязано", "bound"),
        "bind.missing" => ("привязка: код не найден", "bind: code not found"),
        "bind.used" => ("привязка: код уже использован", "bind: code already used"),
        "bind.soon" => ("привязка: ещё не готово", "bind: not ready yet"),
        "bind.refused" => ("привязка: отказ", "bind: refused"),
        "bind.not_artefact" => ("ссылка не артефакт", "link is not an artefact"),
        "bind.client" => ("клиент", "client"),

        "say.no_face" => ("бокс не предложил ни одного лица", "the box offered no face"),
        "say.need_host" => ("нужен хост", "host required"),
        "say.need_login" => ("нужен логин", "login required"),
        "say.need_two" => ("нужны два сервера", "two servers are required"),
        "say.chain_ok" => ("цепь собрана — подключение это релей", "chain is up — the connection is the relay"),
        "say.need_binary" => ("не найден бинарь mirage-vpn", "the mirage-vpn binary is not there"),
        "say.need_port" => ("нужен порт", "port required"),
        "say.no_key" => ("ключа нет", "no key"),
        "say.my_key" => ("ключ в буфере — вставьте его в бокс", "the key is in the clipboard — put it into the box"),
        "say.no_link" => ("нет ссылки", "no link"),
        "say.file_gone" => ("файл недоступен", "file unavailable"),
        "say.need_name" => ("нужно имя", "name required"),
        "say.nothing" => ("нечего отдать", "nothing to share"),
        "say.saved" => ("записано", "saved"),
        "say.forgot" => ("сервер забыт в клиенте", "server forgotten on this client"),
        "say.copied" => ("скопировано", "copied"),
        "say.empty" => ("пусто", "empty"),
        "say.long" => ("слишком длинно", "too long"),
        "say.cfg" => ("конфиг принят", "config accepted"),
        "say.link_ok" => ("ссылка принята", "link accepted"),
        "say.unreadable" => ("файл не читается", "cannot read the file"),
        "say.clip_down" => ("буфер недоступен", "clipboard unavailable"),
        "say.clip_empty" => ("в буфере пусто", "clipboard is empty"),
        "say.bad_image" => ("битый буфер картинки", "bad image buffer"),
        "say.qr_none" => ("QR-код не найден на картинке", "no QR code in the picture"),
        "say.qr_bad" => ("QR не читается", "QR could not be read"),

        "dlg.title" => ("Конфиг или ссылка", "Config or link"),
        "dlg.filter" => ("конфиг", "config"),

        "cam.down" => ("камера недоступна", "camera unavailable"),
        "cam.start" => ("камера не стартует", "camera did not start"),
        "cam.opening" => ("открываю камеру…", "opening the camera…"),
        "cam.scan" => ("сканирую QR", "scanning QR"),
        "cam.cancel" => ("клик чтобы отменить", "click to cancel"),

        // The installer's own copy (`p.*`) lives with the installer, in
        // `mirage_client_chain::i18n` — the GUI and the headless client read
        // it from there, so the two can never say different things.
        _ => (id, id),
    }
}

fn stored() -> Option<Lang> {
    let path = std::env::current_exe().ok()?.parent()?.join("lang");
    let text = std::fs::read_to_string(path).ok()?;
    match text.trim() {
        "ru" => Some(Lang::Ru),
        "en" => Some(Lang::En),
        _ => None,
    }
}

fn write_stored(lang: Lang) -> std::io::Result<()> {
    let path = std::env::current_exe()?.parent().ok_or(std::io::Error::other("no exe dir"))?.join("lang");
    let body = match lang {
        Lang::Ru => "ru\n",
        Lang::En => "en\n",
    };
    std::fs::write(path, body)
}

/// Primary language of the OS interface. Russian and Kazakh open in Russian.
/// Anything else opens in English.
#[cfg(windows)]
fn system() -> Lang {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    match unsafe { GetUserDefaultUILanguage() } & 0x3ff {
        0x19 | 0x3f => Lang::Ru,
        _ => Lang::En,
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn system() -> Lang {
    use std::ffi::{c_void, CStr};
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFLocaleCopyPreferredLanguages() -> *const c_void;
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, idx: isize) -> *const c_void;
        fn CFStringGetCString(s: *const c_void, buf: *mut u8, size: isize, enc: u32) -> bool;
        fn CFRelease(cf: *const c_void);
    }
    const UTF8: u32 = 0x0800_0100;
    unsafe {
        let list = CFLocaleCopyPreferredLanguages();
        if list.is_null() || CFArrayGetCount(list) < 1 {
            if !list.is_null() {
                CFRelease(list);
            }
            return Lang::En;
        }
        let first = CFArrayGetValueAtIndex(list, 0);
        let mut buf = [0u8; 32];
        let ok = CFStringGetCString(first, buf.as_mut_ptr(), buf.len() as isize, UTF8);
        CFRelease(list);
        if !ok {
            return Lang::En;
        }
        let tag = CStr::from_ptr(buf.as_ptr() as *const i8).to_string_lossy();
        let tag = tag.to_ascii_lowercase();
        if tag.starts_with("ru") || tag.starts_with("kk") {
            Lang::Ru
        } else {
            Lang::En
        }
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "ios")))]
fn system() -> Lang {
    Lang::En
}
