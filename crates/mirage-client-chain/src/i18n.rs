//! The installer's copy, in Russian and English.
//!
//! # Why the copy travels with the installer
//!
//! The status lines of a box install ("the box is checking its ports", "the
//! container did not return a link") are the installer's OWN words. They
//! used to live in the GUI's text table, which made the headless client
//! unable to speak them — and a headless client that cannot say what it is
//! doing is a client the operator has to guess at.
//!
//! So the copy lives here, next to the code that speaks it. The GUI reads it
//! from here too: one source of truth, two consumers.
//!
//! # How the language is picked
//!
//! The same way the GUI picks it: a `lang` file beside the executable (`ru`
//! or `en`), and the OS interface language when there is no such file. The
//! file is read on EVERY call and never cached, so a GUI that flips the
//! language mid-session flips it here too — the installer and the window
//! cannot disagree about what language they are speaking.

/// The language the copy is in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// Russian.
    Ru = 0,
    /// English.
    En = 1,
}

/// The copy in the language that is current.
pub fn tr(id: &'static str) -> &'static str {
    let (ru, en) = row(id);
    match current() {
        Lang::Ru => ru,
        Lang::En => en,
    }
}

/// The language in use right now.
///
/// Read from the `lang` file beside the executable every time, so that a
/// language flipped in the GUI is seen by the installer at once.
pub fn current() -> Lang {
    stored().unwrap_or_else(system)
}

/// The name shown on a language row.
pub fn current_name() -> &'static str {
    match current() {
        Lang::Ru => "русский",
        Lang::En => "english",
    }
}

/// The pick the person made, as it is written beside the executable.
fn stored() -> Option<Lang> {
    let path = std::env::current_exe().ok()?.parent()?.join("lang");
    let text = std::fs::read_to_string(path).ok()?;
    match text.trim() {
        "ru" => Some(Lang::Ru),
        "en" => Some(Lang::En),
        _ => None,
    }
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

/// The copy of one key: Russian first, English second.
///
/// An unknown key answers for itself: a line that says "p.no_such_key" is
/// worse than a wrong translation, because it hides the mistake.
fn row(id: &'static str) -> (&'static str, &'static str) {
    match id {
        "p.connect" => ("соединение", "connecting"),
        "p.faces_review" => (
            "бокс предложил лица — проверьте их и нажмите «установить» ещё раз",
            "the box offered faces — check them and press install again",
        ),
        "p.up" => ("сервер поднят", "server is up"),
        "p.removing" => ("снятие", "removing"),
        "p.removed" => ("контейнер снят", "container removed"),
        "p.no_key" => ("нет ключа", "no key"),
        "p.remove_fail" => ("снятие", "remove"),
        "p.key_on" => ("ключ на сервере", "key is on the server"),
        "p.image" => ("образ", "image"),
        "p.ports" => ("бокс смотрит свои порты", "the box is checking its ports"),
        "p.faces_timeout" => (
            "предложение лиц дольше 15 минут",
            "face proposal took longer than 15 minutes",
        ),
        "p.no_proposal" => (
            "бокс не ответил предложением",
            "the box did not answer with a proposal",
        ),
        "p.install_timeout" => (
            "установка дольше 15 минут",
            "install took longer than 15 minutes",
        ),
        "p.container" => ("контейнер", "container"),
        "p.no_link" => (
            "контейнер не отдал ссылку",
            "the container did not return a link",
        ),
        "p.no_faces_doc" => (
            "бокс не прислал предложение лиц",
            "the box did not send a face proposal",
        ),
        "p.faces_bad" => (
            "предложение лиц не прочитано",
            "face proposal could not be read",
        ),
        "p.no_face" => ("бокс не прислал ни одного лица", "the box did not send a face"),
        "p.key_write" => ("не удалось записать ключ", "could not write the key"),
        "p.ssh_pass" => ("ssh: пароль не принят", "ssh: password rejected"),
        "p.pass_retry" => (
            "бокс ещё не готов — пробуем пароль снова",
            "the box is not ready yet — trying the password again",
        ),
        "p.ssh_key" => ("ssh: ключ не принят", "ssh: key rejected"),
        "p.ssh_timeout" => ("таймаут", "timed out"),
        "p.ssh_host" => ("ключ хоста сменился", "host key changed"),
        "p.sftp_size" => ("размер не сошёлся", "size did not match"),
        _ => (id, id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_the_installer_uses_has_copy() {
        // The installer names its keys as literals; a key that fell out of
        // the table would answer with its own name in the status line.
        for id in [
            "p.connect",
            "p.faces_review",
            "p.up",
            "p.removing",
            "p.removed",
            "p.no_key",
            "p.remove_fail",
            "p.key_on",
            "p.image",
            "p.ports",
            "p.faces_timeout",
            "p.no_proposal",
            "p.install_timeout",
            "p.container",
            "p.no_link",
            "p.no_faces_doc",
            "p.faces_bad",
            "p.no_face",
            "p.key_write",
            "p.ssh_pass",
            "p.pass_retry",
            "p.ssh_key",
            "p.ssh_timeout",
            "p.ssh_host",
            "p.sftp_size",
        ] {
            let (ru, en) = row(id);
            assert_ne!(ru, id, "{id} has no Russian copy");
            assert_ne!(en, id, "{id} has no English copy");
        }
    }

    #[test]
    fn an_unknown_key_answers_for_itself() {
        let (ru, en) = row("p.no_such_key");
        assert_eq!(ru, "p.no_such_key");
        assert_eq!(en, "p.no_such_key");
    }
}
