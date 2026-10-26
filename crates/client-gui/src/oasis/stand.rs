//! Standalone sheet. Four sections under the chrome divider: server, protocols,
//! access (link, guests, camera / file / clipboard), connection (kill switch,
//! dns, split). The install itself is [`super::provision`]. Password is never
//! written. Kill switch and the split lists live in the profile; ports, MTU,
//! stack, and dns go out in `server.env` on install. Guest shares are local
//! files, not a server call. A raw link or config is stored as text. A JSON
//! object is what the shell hands to the subscriber.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use qrcode::{Color, QrCode};
use uzor::input::{InputCoordinator, LayerId, Sense, WidgetKind};
use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::{Rect, WidgetId};
use uzor_framework::handle::VisualView;

use super::provision::{self, InstallBus, Job};
use crate::shell::menu::Geom;

const AMBER: &str = "#FBB26A";
const FG: &str = "#F4F4F5";
const DIM: &str = "#6B6F7A";
const IDLE: &str = "#878B91";
const ROW: f64 = 40.0;
const ROW_H: f64 = 32.0;
const NAV_H: f64 = 44.0;
const LABEL_W: f64 = 108.0;
const TOGGLE_W: f64 = 64.0;
const REVOKE_W: f64 = 72.0;

/// The server-side protocols the standalone image publishes, as
/// `(id, port, mtu, on_by_default)`.
///
/// The id names what the traffic should LOOK like, not a protocol to
/// implement: Mirage has one wire over TCP and one over UDP, and the mask on
/// the wire is derived from the port (443 is TLS, 80 is a WebSocket, and so
/// on — the same rule the fleet's own persona follows). A UDP id publishes a
/// datagram face, relayed to the node the same way the fleet does it.
///
/// Two rules the server enforces on this list, and the menu has to honour
/// them or the install fails on the box:
///
/// * **No two entries share a port ON THE SAME TRANSPORT.** The server binds
///   one face per port and refuses a duplicate. The same port NUMBER on TCP
///   and on UDP is not a duplicate — those are separate socket spaces, and an
///   ordinary server serves HTTPS on TCP 443 and QUIC on UDP 443.
/// * **At least one TCP face.** The subscriber client dials TCP entrances
///   only — a box with nothing but datagram faces has nothing to dial.
///
/// `quic` is the UDP id for the port where QUIC usually lives. It is named
/// after the PORT's neighbourhood, not after the wire shape: with no mask
/// named it is the perimeter's datagram door, and with a QUIC mask named
/// (`{ID}_MASK`) it is an endpoint the node holds, speaking real QUIC
/// outside and Mirage inside.
const CATALOG: &[(&str, &str, &str, bool)] = &[
    ("cloak", "443", "", true),
    ("xray", "8443", "", false),
    ("shadowsocks", "2083", "", false),
    ("socks5", "1080", "", false),
    ("awg", "853", "1280", false),
    ("wireguard", "3478", "1420", false),
    ("openvpn", "5060", "", false),
    ("ikev2", "5349", "", false),
    ("quic", "443", "", false),
];

/// Which catalogue id each kind of proposed face wears.
///
/// The id is a SLOT NAME, not a description: the mask written next to it in
/// `server.env` is what the traffic actually looks like. The pools are the
/// catalogue's own ids, grouped by the kind of face they name, so a proposal
/// lands on ids the server already knows.
///
/// The kinds are the mask's own serde tag — `web_socket`, `wire_guard` and
/// the rest are spelled the way `TransportCamouflage` serializes them, not
/// the way its `label()` prints them.
const ID_POOLS: &[(&str, &[&str])] = &[
    ("tls_real", &["cloak", "xray"]),
    ("web_socket", &["shadowsocks"]),
    ("xhttp", &["socks5"]),
    ("quic", &["quic"]),
    ("http3", &["quic"]),
    ("web_transport", &["quic"]),
    ("wire_guard", &["quic"]),
    ("hysteria2", &["quic"]),
    ("raw_udp", &["quic", "awg", "wireguard", "openvpn", "ikev2"]),
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Server,
    Protocols,
    Access,
    Link,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    None,
    Host,
    User,
    Pass,
    Link,
    Guest,
    Dns,
    Except,
    Site,
    App,
    Port,
    Mtu,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kill {
    Off,
    Soft,
    Strict,
}

pub struct Proto {
    pub id: &'static str,
    pub on: bool,
    pub port: String,
    pub mtu: String,
    /// The mask this face wears, as the JSON the server reads it back from.
    ///
    /// Empty means "no mask named" — the server derives one from the port,
    /// the way it always did. Filled means the person (or the box's own
    /// proposal) named the mask, and it is written into `server.env` as it
    /// stands.
    pub mask: String,
    /// Why this face is offered, in the operator's language.
    ///
    /// The box's own proposal says it; the catalogue's own rows have none.
    pub why: String,
}

struct Guest {
    name: String,
    full: bool,
    body: String,
}

pub struct Stand {
    pub section: Section,
    pub host: String,
    pub user: String,
    pub pass: String,
    pub stack_auto: bool,
    pub stack: String,
    pub protocols: Vec<Proto>,
    pub focus: Focus,
    pub link: String,
    guests: Vec<Guest>,
    guest_draft: String,
    qr_guest: Option<usize>,
    kill: Kill,
    pub dns: String,
    pub except: String,
    split_bypass: bool,
    sites: Vec<String>,
    apps: Vec<String>,
    site_draft: String,
    app_draft: String,
    pub bus: Arc<InstallBus>,
    /// Bumped when an install takes the password, so each window clears that field once.
    pub pass_epoch: u64,
    /// Which draft was just committed. Each window clears that field store once.
    pub clear_which: Focus,
    pub clear_epoch: u64,
    /// Sites, apps, or the split side changed. The shell pushes `set_scope` and clears this.
    pub scope_dirty: bool,
    /// The shell should drop the last adopt and read `link` again.
    relink: bool,
    /// Bumped when `link` is replaced from outside the field, so each window pushes it once.
    link_epoch: u64,
    /// The shell opens the camera on the next frame.
    scan_request: bool,
    /// The box has offered its faces, and the list on screen is the
    /// person's answer to that offer.
    pub faces_from_box: bool,
}

impl Stand {
    pub fn load() -> Self {
        let mut stand = Self {
            section: Section::Server,
            host: String::new(),
            user: "root".into(),
            pass: String::new(),
            stack_auto: true,
            stack: String::new(),
            protocols: default_protocols(),
            focus: Focus::None,
            link: String::new(),
            guests: Vec::new(),
            guest_draft: String::new(),
            qr_guest: None,
            kill: Kill::Off,
            dns: String::new(),
            except: String::new(),
            split_bypass: false,
            sites: Vec::new(),
            apps: Vec::new(),
            site_draft: String::new(),
            app_draft: String::new(),
            bus: Arc::new(InstallBus::new()),
            pass_epoch: 0,
            clear_which: Focus::None,
            clear_epoch: 0,
            scope_dirty: false,
            relink: false,
            link_epoch: 0,
            scan_request: false,
            faces_from_box: false,
        };
        if let Some(dir) = dir() {
            if let Ok(text) = std::fs::read_to_string(dir.join("host.json")) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    stand.section = match v.get("section").and_then(|s| s.as_str()) {
                        Some("protocols") => Section::Protocols,
                        Some("access") => Section::Access,
                        Some("link") => Section::Link,
                        _ => Section::Server,
                    };
                    if let Some(h) = v.get("host").and_then(|s| s.as_str()) {
                        stand.host = h.to_owned();
                    }
                    if let Some(u) = v.get("user").and_then(|s| s.as_str()) {
                        if !u.is_empty() {
                            stand.user = u.to_owned();
                        }
                    }
                    if let Some(auto) = v.get("stack_auto").and_then(|s| s.as_bool()) {
                        stand.stack_auto = auto;
                    }
                    if let Some(st) = v.get("stack").and_then(|s| s.as_str()) {
                        stand.stack = st.to_owned();
                    }
                    if let Some(arr) = v.get("protocols").and_then(|p| p.as_array()) {
                        for item in arr {
                            let Some(id) = item.get("id").and_then(|s| s.as_str()) else {
                                continue;
                            };
                            let Some(proto) = stand.protocols.iter_mut().find(|p| p.id == id) else {
                                continue;
                            };
                            if let Some(on) = item.get("on").and_then(|b| b.as_bool()) {
                                proto.on = on;
                            }
                            if let Some(port) = item.get("port").and_then(|s| s.as_str()) {
                                proto.port = port.to_owned();
                            }
                            if let Some(mtu) = item.get("mtu").and_then(|s| s.as_str()) {
                                proto.mtu = mtu.to_owned();
                            }
                            if let Some(mask) = item.get("mask").and_then(|s| s.as_str()) {
                                proto.mask = mask.to_owned();
                            }
                        }
                    }
                    stand.kill = match v.get("kill").and_then(|s| s.as_str()) {
                        Some("soft") => Kill::Soft,
                        Some("strict") => Kill::Strict,
                        _ => Kill::Off,
                    };
                    if let Some(dns) = v.get("dns").and_then(|s| s.as_str()) {
                        stand.dns = dns.to_owned();
                    }
                    if let Some(except) = v.get("except").and_then(|s| s.as_str()) {
                        stand.except = except.to_owned();
                    }
                    if let Some(bypass) = v.get("split_bypass").and_then(|s| s.as_bool()) {
                        stand.split_bypass = bypass;
                    }
                    if let Some(sites) = string_list(&v, "sites") {
                        stand.sites = sites;
                    }
                    if let Some(apps) = string_list(&v, "apps") {
                        stand.apps = apps;
                    }
                }
            }
            if let Ok(link) = std::fs::read_to_string(dir.join("link.txt")) {
                stand.link = link.trim().to_owned();
            }
            if let Ok(text) = std::fs::read_to_string(dir.join("guests.json")) {
                if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                    stand.guests = arr
                        .into_iter()
                        .filter_map(|item| {
                            let name = item.get("name")?.as_str()?.trim().to_owned();
                            if name.is_empty() {
                                return None;
                            }
                            Some(Guest {
                                name,
                                full: item.get("full").and_then(|b| b.as_bool()).unwrap_or(false),
                                body: item.get("body").and_then(|s| s.as_str()).unwrap_or("").to_owned(),
                            })
                        })
                        .collect();
                }
            }
        }
        stand
    }

    pub fn save_profile(&self) {
        let Some(dir) = dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let protocols: Vec<_> = self
            .protocols
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "on": p.on,
                    "port": p.port,
                    "mtu": p.mtu,
                    "mask": p.mask,
                })
            })
            .collect();
        let text = serde_json::json!({
            "section": section_key(self.section),
            "host": self.host,
            "user": self.user,
            "stack_auto": self.stack_auto,
            "stack": self.stack,
            "protocols": protocols,
            "kill": kill_key(self.kill),
            "dns": self.dns,
            "except": self.except,
            "split_bypass": self.split_bypass,
            "sites": self.sites,
            "apps": self.apps,
        });
        if let Ok(body) = serde_json::to_string_pretty(&text) {
            let _ = std::fs::write(dir.join("host.json"), body);
        }
    }

    fn save_guests(&self) {
        let Some(dir) = dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let guests: Vec<_> = self
            .guests
            .iter()
            .map(|g| serde_json::json!({"name": g.name, "full": g.full, "body": g.body}))
            .collect();
        if let Ok(body) = serde_json::to_string_pretty(&guests) {
            let _ = std::fs::write(dir.join("guests.json"), body);
        }
    }

    pub fn pull_bus(&mut self) {
        let faces = self.bus.faces.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(faces) = faces {
            self.apply_faces(&faces);
        }
        let published = self.bus.link.lock().unwrap_or_else(|p| p.into_inner()).take();
        let Some(link) = published else {
            return;
        };
        if self.link == link {
            return;
        }
        self.link = link;
        self.bump_link_epoch();
        self.save_link();
    }

    /// Put the box's proposal into the protocol list.
    ///
    /// The box says what it COULD publish; this is where the person looks at
    /// it. Every proposed face lands in the list switched ON — the box
    /// checked these ports, and the person turns off what they do not want.
    /// The catalogue's own rows that the proposal did not name stay in the
    /// list, switched OFF, for a person who wants something else.
    pub fn apply_faces(&mut self, faces: &[provision::ProposedFace]) {
        let mut taken: Vec<&'static str> = Vec::new();
        let mut applied = Vec::new();
        for face in faces {
            let Some(id) = id_for_face(face, &taken) else {
                continue;
            };
            taken.push(id);
            applied.push(Proto {
                id,
                on: true,
                port: face.port.to_string(),
                mtu: String::new(),
                mask: face.mask.clone(),
                why: face.why.clone(),
            });
        }
        if applied.is_empty() {
            self.say("бокс не предложил ни одного лица");
            return;
        }
        // The proposed rows go first — they are what the box can do now —
        // and the catalogue's own rows follow, switched off, for a person
        // who wants something else.
        let mut ordered = applied;
        for proto in &self.protocols {
            if ordered.iter().any(|p| p.id == proto.id) {
                continue;
            }
            ordered.push(Proto {
                id: proto.id,
                on: false,
                port: proto.port.clone(),
                mtu: proto.mtu.clone(),
                mask: String::new(),
                why: String::new(),
            });
        }
        self.protocols = ordered;
        self.faces_from_box = true;
        self.save_profile();
    }

    pub fn link_epoch(&self) -> u64 {
        self.link_epoch
    }

    pub fn status(&self) -> String {
        self.bus.text.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn say(&self, line: &str) {
        *self.bus.text.lock().unwrap_or_else(|p| p.into_inner()) = line.to_owned();
    }

    pub fn set_section(&mut self, section: Section) {
        if self.section == section {
            return;
        }
        self.section = section;
        self.focus = Focus::None;
        self.save_profile();
    }

    pub fn take_scope(&mut self) -> bool {
        let dirty = self.scope_dirty;
        self.scope_dirty = false;
        dirty
    }

    /// Include-list when the toggle is "только эти", bypass when it is "все кроме".
    pub fn scope_args(&self) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
        if self.split_bypass {
            (Vec::new(), Vec::new(), self.sites.clone(), self.apps.clone())
        } else {
            (self.sites.clone(), self.apps.clone(), Vec::new(), Vec::new())
        }
    }

    pub fn server_env(&self) -> String {
        let stack = if self.stack_auto || self.stack.trim().is_empty() {
            "auto"
        } else {
            self.stack.trim()
        };
        let mut on: Vec<&Proto> = self.protocols.iter().filter(|p| p.on).collect();
        if on.is_empty() {
            // Nothing switched on: fall back to the catalogue's first entry
            // rather than to a protocol by name — the menu owns that choice,
            // and a name that leaves the catalogue would break the install.
            if let Some(first) = self.protocols.first() {
                on.push(first);
            }
        }
        let mut lines = Vec::new();
        lines.push(format!("STACK={stack}"));
        let ids: Vec<&str> = on.iter().map(|p| p.id).collect();
        lines.push(format!("PROTOCOLS={}", ids.join(",")));
        for proto in &on {
            let key = proto.id.to_ascii_uppercase().replace('-', "_");
            if !proto.port.trim().is_empty() {
                lines.push(format!("{key}_PORT={}", proto.port.trim()));
            }
            if !proto.mtu.trim().is_empty() {
                lines.push(format!("{key}_MTU={}", proto.mtu.trim()));
            }
            // The mask, when one was named. The server reads it back as the
            // JSON the box wrote, and points it at this face's own port.
            if !proto.mask.trim().is_empty() {
                lines.push(format!("{key}_MASK={}", proto.mask.trim()));
            }
        }
        if !self.dns.trim().is_empty() {
            lines.push(format!("DNS={}", self.dns.trim()));
        }
        lines.push(String::new());
        lines.join("\n")
    }

    pub fn install(&mut self) {
        if self.bus.running.load(Ordering::Relaxed) {
            return;
        }
        let Some((host, port)) = split_host(&self.host) else {
            self.say("нужен хост");
            return;
        };
        if self.user.trim().is_empty() {
            self.say("нужен логин");
            return;
        }
        self.save_profile();
        let env = self.server_env();
        let stack = if self.stack_auto || self.stack.trim().is_empty() {
            "auto".to_owned()
        } else {
            self.stack.trim().to_owned()
        };
        self.pass_epoch = self.pass_epoch.wrapping_add(1);
        if self.pass_epoch == 0 {
            self.pass_epoch = 1;
        }
        provision::start(
            Job {
                host,
                port,
                user: self.user.trim().to_owned(),
                password: std::mem::take(&mut self.pass),
                stack,
                env,
                // The first press asks the box what it could publish; the
                // second one installs with the faces the person chose.
                ask_faces: !self.faces_from_box,
            },
            Arc::clone(&self.bus),
        );
    }

    pub fn remove_remote(&mut self) {
        if self.bus.running.load(Ordering::Relaxed) {
            return;
        }
        let Some((host, port)) = split_host(&self.host) else {
            self.say("нужен хост");
            return;
        };
        if self.user.trim().is_empty() {
            self.say("нужен логин");
            return;
        }
        self.save_profile();
        provision::remove(host, port, self.user.trim().to_owned(), Arc::clone(&self.bus));
    }

    pub fn export(&self) {
        if self.link.is_empty() {
            self.say("нет ссылки");
            return;
        }
        let Some(path) = rfd::FileDialog::new().set_file_name("m1r4g3-link.txt").save_file() else {
            return;
        };
        let _ = std::fs::write(path, &self.link);
    }

    pub fn copy_link(&self) {
        if self.link.is_empty() {
            self.say("нет ссылки");
            return;
        }
        self.copy_text(&self.link);
    }

    pub fn toggle_stack(&mut self) {
        self.stack_auto = !self.stack_auto;
        let fallback = self.protocols.first().map(|p| p.id.to_owned()).unwrap_or_default();
        if !self.stack_auto && !self.protocols.iter().any(|p| p.id == self.stack) {
            self.stack = self
                .protocols
                .iter()
                .find(|p| p.on)
                .map(|p| p.id.to_owned())
                .unwrap_or(fallback);
        }
        self.focus = Focus::None;
        self.save_profile();
    }

    pub fn select_proto(&mut self, index: usize) {
        let Some(id) = self.protocols.get(index).map(|p| p.id) else {
            return;
        };
        self.stack_auto = false;
        self.stack = id.to_owned();
        if let Some(proto) = self.protocols.get_mut(index) {
            proto.on = true;
        }
        self.focus = Focus::None;
        self.save_profile();
    }

    pub fn toggle_proto(&mut self, index: usize) {
        let Some(proto) = self.protocols.get_mut(index) else {
            return;
        };
        proto.on = !proto.on;
        self.save_profile();
    }

    pub fn cycle_kill(&mut self) {
        self.kill = match self.kill {
            Kill::Off => Kill::Soft,
            Kill::Soft => Kill::Strict,
            Kill::Strict => Kill::Off,
        };
        self.save_profile();
    }

    pub fn toggle_split(&mut self) {
        self.split_bypass = !self.split_bypass;
        self.scope_dirty = true;
        self.save_profile();
    }

    pub fn commit_site(&mut self) -> bool {
        let text = self.site_draft.trim().to_owned();
        if text.is_empty() {
            return false;
        }
        if !self.sites.iter().any(|s| s == &text) {
            self.sites.push(text);
        }
        self.site_draft.clear();
        self.bump_clear(Focus::Site);
        self.scope_dirty = true;
        self.save_profile();
        true
    }

    pub fn commit_app(&mut self) -> bool {
        let text = self.app_draft.trim().to_owned();
        if text.is_empty() {
            return false;
        }
        if !self.apps.iter().any(|s| s == &text) {
            self.apps.push(text);
        }
        self.app_draft.clear();
        self.bump_clear(Focus::App);
        self.scope_dirty = true;
        self.save_profile();
        true
    }

    pub fn drop_site(&mut self, index: usize) {
        if index >= self.sites.len() {
            return;
        }
        self.sites.remove(index);
        self.scope_dirty = true;
        self.save_profile();
    }

    pub fn drop_app(&mut self, index: usize) {
        if index >= self.apps.len() {
            return;
        }
        self.apps.remove(index);
        self.scope_dirty = true;
        self.save_profile();
    }

    /// Local share. Connect is the current link. Full is host, user, link, and
    /// stack — never the password. Nothing is sent to the server.
    pub fn invite(&mut self, full: bool) {
        let name = self.guest_draft.trim().to_owned();
        if name.is_empty() {
            self.say("нужно имя");
            return;
        }
        let body = if full {
            if self.host.trim().is_empty() {
                self.say("нечего отдать");
                return;
            }
            let stack = if self.stack_auto || self.stack.trim().is_empty() {
                "auto"
            } else {
                self.stack.trim()
            };
            serde_json::json!({
                "host": self.host.trim(),
                "user": self.user.trim(),
                "link": self.link,
                "stack": stack,
            })
            .to_string()
        } else if self.link.trim().is_empty() {
            self.say("нет ссылки");
            return;
        } else {
            self.link.clone()
        };
        self.guests.push(Guest { name, full, body: body.clone() });
        self.qr_guest = Some(self.guests.len() - 1);
        self.guest_draft.clear();
        self.bump_clear(Focus::Guest);
        self.save_guests();
        self.copy_text(&body);
        self.say("записано");
    }

    pub fn select_guest(&mut self, index: usize) {
        let Some(body) = self.guests.get(index).map(|g| g.body.clone()) else {
            return;
        };
        self.qr_guest = Some(index);
        if !body.is_empty() {
            self.copy_text(&body);
            self.say("скопировано");
        }
    }

    pub fn revoke(&mut self, index: usize) {
        if index >= self.guests.len() {
            return;
        }
        self.guests.remove(index);
        self.qr_guest = match self.qr_guest {
            Some(i) if i == index => None,
            Some(i) if i > index => Some(i - 1),
            other => other,
        };
        self.save_guests();
    }

    pub fn set_link(&mut self, link: &str) {
        let link = link.trim();
        if link.is_empty() || link.len() > 256 * 1024 {
            return;
        }
        self.link = link.to_owned();
        self.focus = Focus::Link;
        self.qr_guest = None;
        self.relink = true;
        self.bump_link_epoch();
        self.save_link();
    }

    pub fn mark_relink(&mut self) {
        self.relink = true;
    }

    pub fn take_relink(&mut self) -> bool {
        let relink = self.relink;
        self.relink = false;
        relink
    }

    pub fn request_scan(&mut self) {
        self.scan_request = true;
    }

    pub fn take_scan_request(&mut self) -> bool {
        let requested = self.scan_request;
        self.scan_request = false;
        requested
    }

    /// Store a link or a config as text. Empty and oversized payloads stay out of the field.
    pub fn accept_raw(&mut self, raw: &str) {
        let raw = raw.trim();
        if raw.is_empty() {
            self.say("пусто");
            return;
        }
        if raw.len() > 256 * 1024 {
            self.say("слишком длинно");
            return;
        }
        if self.section != Section::Access {
            self.section = Section::Access;
            self.save_profile();
        }
        self.set_link(raw);
        self.say(if raw.starts_with('{') { "конфиг принят" } else { "ссылка принята" });
    }

    pub fn import_file(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Конфиг или ссылка")
            .add_filter("конфиг", &["json", "txt", "mirage", "conf", "vpn"])
            .pick_file()
        else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(raw) => self.accept_raw(&raw),
            Err(_) => self.say("файл не читается"),
        }
    }

    pub fn import_clipboard(&mut self) {
        let mut clip = match arboard::Clipboard::new() {
            Ok(clip) => clip,
            Err(_) => {
                self.say("буфер недоступен");
                return;
            }
        };
        if let Ok(text) = clip.get_text() {
            if !text.trim().is_empty() {
                self.accept_raw(&text);
                return;
            }
        }
        match clip.get_image() {
            Ok(img) => match decode_qr_rgba(&img.bytes, img.width, img.height) {
                Ok(raw) => self.accept_raw(&raw),
                Err(err) => self.say(&err),
            },
            Err(_) => self.say("в буфере пусто"),
        }
    }

    pub fn save_link(&self) {
        let Some(dir) = dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("link.txt"), &self.link);
    }

    pub fn field_mut(&mut self, focus: Focus) -> Option<&mut String> {
        match focus {
            Focus::Host => Some(&mut self.host),
            Focus::User => Some(&mut self.user),
            Focus::Pass => Some(&mut self.pass),
            Focus::Link => Some(&mut self.link),
            Focus::Guest => Some(&mut self.guest_draft),
            Focus::Dns => Some(&mut self.dns),
            Focus::Except => Some(&mut self.except),
            Focus::Site => Some(&mut self.site_draft),
            Focus::App => Some(&mut self.app_draft),
            Focus::Port => {
                let i = self.active_index()?;
                Some(&mut self.protocols[i].port)
            }
            Focus::Mtu => {
                let i = self.active_index()?;
                Some(&mut self.protocols[i].mtu)
            }
            Focus::None => None,
        }
    }

    pub fn type_text(&mut self, text: &str) {
        let focus = self.focus;
        let limit = match focus {
            Focus::Link => 256 * 1024,
            Focus::Port => 8,
            Focus::Mtu => 5,
            Focus::Guest => 64,
            Focus::None => return,
            _ => 240,
        };
        {
            let Some(buf) = self.buf_mut() else {
                return;
            };
            if buf.len() + text.len() > limit {
                return;
            }
            buf.push_str(text);
        }
        if focus == Focus::Link {
            self.save_link();
        }
    }

    pub fn backspace(&mut self) {
        if let Some(buf) = self.buf_mut() {
            buf.pop();
        }
    }

    fn buf_mut(&mut self) -> Option<&mut String> {
        self.field_mut(self.focus)
    }

    fn active_index(&self) -> Option<usize> {
        if self.stack_auto {
            return None;
        }
        self.protocols.iter().position(|p| p.id == self.stack)
    }

    /// Why the box offered the face the person is looking at.
    ///
    /// The face the person is on, if there is one; otherwise the first
    /// proposed face that is switched on. Only proposed faces carry a
    /// reason — the catalogue's own rows do not, and there is nothing to
    /// say about them.
    fn active_why(&self) -> Option<String> {
        let proto = match self.active_index() {
            Some(index) => self.protocols.get(index),
            None => self
                .protocols
                .iter()
                .find(|p| p.on && !p.why.trim().is_empty()),
        }?;
        let why = proto.why.trim();
        if why.is_empty() {
            return None;
        }
        Some(why.to_owned())
    }

    fn bump_clear(&mut self, which: Focus) {
        self.clear_which = which;
        self.clear_epoch = self.clear_epoch.wrapping_add(1);
        if self.clear_epoch == 0 {
            self.clear_epoch = 1;
        }
    }

    fn bump_link_epoch(&mut self) {
        self.link_epoch = self.link_epoch.wrapping_add(1);
        if self.link_epoch == 0 {
            self.link_epoch = 1;
        }
    }

    fn copy_text(&self, text: &str) {
        if let Ok(mut clip) = arboard::Clipboard::new() {
            let _ = clip.set_text(text.to_owned());
        }
    }
}

fn default_protocols() -> Vec<Proto> {
    CATALOG
        .iter()
        .map(|(id, port, mtu, on)| Proto {
            id,
            on: *on,
            port: (*port).to_owned(),
            mtu: (*mtu).to_owned(),
            mask: String::new(),
            why: String::new(),
        })
        .collect()
}

/// The catalogue id a proposed face wears.
///
/// The mask's own kind picks the pool (`tls_real` wears `cloak` or `xray`,
/// and so on), and the first id in that pool that nobody has taken yet is
/// the one. A kind the pools do not know falls back to the first free
/// catalogue id of all, so a face is never dropped for want of a name.
fn id_for_face(face: &provision::ProposedFace, taken: &[&'static str]) -> Option<&'static str> {
    let kind = serde_json::from_str::<serde_json::Value>(&face.mask)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_default();
    if let Some((_, pool)) = ID_POOLS.iter().find(|(name, _)| *name == kind) {
        if let Some(id) = pool.iter().copied().find(|id| !taken.contains(id)) {
            return Some(id);
        }
    }
    CATALOG
        .iter()
        .map(|(id, ..)| *id)
        .find(|id| !taken.contains(id))
}

fn section_key(section: Section) -> &'static str {
    match section {
        Section::Server => "server",
        Section::Protocols => "protocols",
        Section::Access => "access",
        Section::Link => "link",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A proposed face, as the box would write it.
    fn face(mask: &str, port: u16) -> provision::ProposedFace {
        provision::ProposedFace {
            port,
            transport: "tcp".to_owned(),
            mask: mask.to_owned(),
            why: "why".to_owned(),
        }
    }

    /// The two TLS faces take the two TLS ids, in order.
    #[test]
    fn two_tls_faces_take_the_two_tls_ids() {
        let first = face(r#"{"type":"tls_real","sni":"a.example","alpn":["h2"]}"#, 443);
        let second = face(r#"{"type":"tls_real","sni":"a.example","alpn":["h2"]}"#, 8443);
        let mut taken: Vec<&'static str> = Vec::new();
        let one = id_for_face(&first, &taken).expect("an id");
        taken.push(one);
        let two = id_for_face(&second, &taken).expect("an id");
        assert_eq!((one, two), ("cloak", "xray"));
    }

    /// The mask's kind picks the id — a WebSocket face is not a TLS face,
    /// whatever port it sits on.
    #[test]
    fn the_kind_picks_the_id() {
        let websocket = face(r#"{"type":"web_socket","path":"/","host":"a.example"}"#, 80);
        let xhttp = face(r#"{"type":"xhttp","path":"/","host":"a.example"}"#, 8080);
        let quic = face(r#"{"type":"quic","alpn":["h3"],"bind_addr":"0.0.0.0:443"}"#, 443);
        let taken: Vec<&'static str> = Vec::new();
        assert_eq!(id_for_face(&websocket, &taken), Some("shadowsocks"));
        assert_eq!(id_for_face(&xhttp, &taken), Some("socks5"));
        assert_eq!(id_for_face(&quic, &taken), Some("quic"));
    }

    /// Every id a face can get is one the server already knows.
    #[test]
    fn every_id_is_a_catalogue_id() {
        for (_, pool) in ID_POOLS {
            for id in *pool {
                assert!(
                    CATALOG.iter().any(|(catalog, ..)| catalog == id),
                    "{id} is not in the catalogue"
                );
            }
        }
    }
}

fn kill_key(kill: Kill) -> &'static str {
    match kill {
        Kill::Off => "off",
        Kill::Soft => "soft",
        Kill::Strict => "strict",
    }
}

fn string_list(v: &serde_json::Value, key: &str) -> Option<Vec<String>> {
    let arr = v.get(key)?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|item| item.as_str())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

fn dir() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("standalone"))
}

fn decode_qr_rgba(rgba: &[u8], w: usize, h: usize) -> Result<String, String> {
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return Err("битый буфер картинки".into());
    }
    let mut prep = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| {
        let i = (y * w + x) * 4;
        let (r, g, b) = (rgba[i] as u32, rgba[i + 1] as u32, rgba[i + 2] as u32);
        ((r * 299 + g * 587 + b * 114) / 1000) as u8
    });
    let grids = prep.detect_grids();
    let grid = grids.first().ok_or("QR-код не найден на картинке")?;
    let (_meta, content) = grid.decode().map_err(|e| format!("QR не читается: {e}"))?;
    Ok(content)
}

fn split_host(raw: &str) -> Option<(String, u16)> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some((host, port)) = raw.rsplit_once(':') {
        if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(port) = port.parse::<u16>() {
                if port > 0 {
                    return Some((host.to_owned(), port));
                }
            }
        }
    }
    Some((raw.to_owned(), 22))
}

#[derive(Clone, Copy)]
pub struct FieldSpec {
    pub id: &'static str,
    pub rect: Rect,
    pub focus: Focus,
    pub placeholder: &'static str,
    pub max: usize,
    pub masked: bool,
}

#[derive(Clone, Copy)]
struct NavBtn {
    id: &'static str,
    label: &'static str,
    section: Section,
    rect: Rect,
}

enum ButtonText {
    Install,
    Remove,
    Export,
    Copy,
    Scan,
    File,
    Clip,
    InviteConn,
    InviteFull,
}

enum Choice {
    Stack,
    Kill,
    Split,
}

enum Row {
    Field { rect: Rect, label: &'static str, focus: Focus },
    Button { rect: Rect, text: ButtonText },
    Choice { rect: Rect, label: &'static str, which: Choice },
    Proto { rect: Rect, index: usize, toggle: Rect },
    Note { rect: Rect, text: String },
    Guest { rect: Rect, index: usize, revoke: Rect },
    Entry { rect: Rect, index: usize, site: bool },
    Line { x: f64, y: f64 },
}

enum HitKind {
    Install,
    Remove,
    Export,
    Copy,
    Scan,
    File,
    Clip,
    Stack,
    Kill,
    Split,
    Invite(bool),
    Proto(usize),
    ProtoOn(usize),
    Guest(usize),
    Revoke(usize),
    DropSite(usize),
    DropApp(usize),
}

struct Hit {
    id: String,
    rect: Rect,
    kind: HitKind,
}

struct Layout {
    fields: Vec<FieldSpec>,
    rows: Vec<Row>,
    hits: Vec<Hit>,
    bottom: f64,
}

struct Lay {
    x: f64,
    y: f64,
    w: f64,
    fields: Vec<FieldSpec>,
    rows: Vec<Row>,
    hits: Vec<Hit>,
}

impl Lay {
    fn row_rect(&mut self) -> Rect {
        let rect = Rect::new(self.x, self.y, self.w, ROW_H);
        self.y += ROW;
        rect
    }

    fn hit(&mut self, id: String, rect: Rect, kind: HitKind) {
        self.hits.push(Hit { id, rect, kind });
    }

    fn field(
        &mut self,
        id: &'static str,
        label: &'static str,
        focus: Focus,
        placeholder: &'static str,
        max: usize,
        masked: bool,
    ) {
        let rect = self.row_rect();
        self.rows.push(Row::Field { rect, label, focus });
        self.fields.push(FieldSpec {
            id,
            rect: Rect::new(rect.x + LABEL_W, rect.y, (rect.width - LABEL_W).max(0.0), rect.height),
            focus,
            placeholder,
            max,
            masked,
        });
    }

    fn pair(
        &mut self,
        left: (&'static str, ButtonText, HitKind),
        right: (&'static str, ButtonText, HitKind),
    ) {
        let y = self.y;
        let half = ((self.w - 8.0) / 2.0).max(0.0);
        let a = Rect::new(self.x, y, half, ROW_H);
        let b = Rect::new(self.x + half + 8.0, y, half, ROW_H);
        self.y += ROW;
        self.rows.push(Row::Button { rect: a, text: left.1 });
        self.rows.push(Row::Button { rect: b, text: right.1 });
        self.hit(left.0.to_owned(), a, left.2);
        self.hit(right.0.to_owned(), b, right.2);
    }

    fn thirds(
        &mut self,
        a: (&'static str, ButtonText, HitKind),
        b: (&'static str, ButtonText, HitKind),
        c: (&'static str, ButtonText, HitKind),
    ) {
        let y = self.y;
        let gap = 8.0;
        let w = ((self.w - gap * 2.0) / 3.0).max(0.0);
        let ra = Rect::new(self.x, y, w, ROW_H);
        let rb = Rect::new(self.x + w + gap, y, w, ROW_H);
        let rc = Rect::new(self.x + (w + gap) * 2.0, y, w, ROW_H);
        self.y += ROW;
        self.rows.push(Row::Button { rect: ra, text: a.1 });
        self.rows.push(Row::Button { rect: rb, text: b.1 });
        self.rows.push(Row::Button { rect: rc, text: c.1 });
        self.hit(a.0.to_owned(), ra, a.2);
        self.hit(b.0.to_owned(), rb, b.2);
        self.hit(c.0.to_owned(), rc, c.2);
    }

    fn choice(&mut self, id: &'static str, label: &'static str, which: Choice, kind: HitKind) {
        let rect = self.row_rect();
        self.rows.push(Row::Choice { rect, label, which });
        self.hit(id.to_owned(), rect, kind);
    }

    fn proto(&mut self, index: usize) {
        let rect = self.row_rect();
        let toggle_w = TOGGLE_W.min(rect.width);
        let toggle = Rect::new(rect.x + rect.width - toggle_w, rect.y, toggle_w, rect.height);
        let body = Rect::new(rect.x, rect.y, (rect.width - toggle_w).max(0.0), rect.height);
        self.rows.push(Row::Proto { rect, index, toggle });
        self.hit(format!("st-p{index}"), body, HitKind::Proto(index));
        self.hit(format!("st-p{index}-on"), toggle, HitKind::ProtoOn(index));
    }

    /// A line of explanation under the list — the box's own reason for the
    /// face the person is looking at.
    fn note(&mut self, text: String) {
        let rect = self.row_rect();
        self.rows.push(Row::Note { rect, text });
    }

    fn guest(&mut self, index: usize) {
        let rect = self.row_rect();
        let revoke_w = REVOKE_W.min(rect.width);
        let revoke = Rect::new(rect.x + rect.width - revoke_w, rect.y, revoke_w, rect.height);
        let body = Rect::new(rect.x, rect.y, (rect.width - revoke_w).max(0.0), rect.height);
        self.rows.push(Row::Guest { rect, index, revoke });
        self.hit(format!("st-g{index}"), body, HitKind::Guest(index));
        self.hit(format!("st-g{index}-x"), revoke, HitKind::Revoke(index));
    }

    fn entry(&mut self, index: usize, site: bool) {
        let rect = self.row_rect();
        self.rows.push(Row::Entry { rect, index, site });
        if site {
            self.hit(format!("st-site-row-{index}"), rect, HitKind::DropSite(index));
        } else {
            self.hit(format!("st-app-row-{index}"), rect, HitKind::DropApp(index));
        }
    }
}

fn layout(stand: &Stand, content: Rect, show_line: bool) -> Layout {
    let mut lay = Lay {
        x: content.x + 16.0,
        y: content.y + NAV_H,
        w: (content.width - 32.0).max(0.0),
        fields: Vec::new(),
        rows: Vec::new(),
        hits: Vec::new(),
    };
    match stand.section {
        Section::Server => {
            lay.field("st-host", "хост", Focus::Host, "хост", 240, false);
            lay.field("st-user", "логин", Focus::User, "логин", 240, false);
            lay.field("st-pass", "пароль", Focus::Pass, "пароль", 240, true);
            lay.pair(
                ("st-go", ButtonText::Install, HitKind::Install),
                ("st-down", ButtonText::Remove, HitKind::Remove),
            );
        }
        Section::Protocols => {
            lay.choice("st-stack", "стек", Choice::Stack, HitKind::Stack);
            for index in 0..stand.protocols.len() {
                lay.proto(index);
            }
            if stand.active_index().is_some() {
                lay.field("st-port", "порт", Focus::Port, "порт", 8, false);
                lay.field("st-mtu", "mtu", Focus::Mtu, "mtu", 5, false);
                // The box's own reason for the face the person is on. It is
                // the sentence that says what this port looks like from
                // outside — and, when the port was not the one first in
                // mind, what happened to it.
                if let Some(why) = stand.active_why() {
                    lay.note(why);
                }
            }
        }
        Section::Access => {
            lay.field("st-link", "ссылка", Focus::Link, "вставить", 256 * 1024, false);
            lay.pair(
                ("st-export", ButtonText::Export, HitKind::Export),
                ("st-copy", ButtonText::Copy, HitKind::Copy),
            );
            lay.thirds(
                ("st-scan", ButtonText::Scan, HitKind::Scan),
                ("st-file", ButtonText::File, HitKind::File),
                ("st-clip", ButtonText::Clip, HitKind::Clip),
            );
            lay.field("st-guest", "гость", Focus::Guest, "имя", 64, false);
            lay.pair(
                ("st-invite-conn", ButtonText::InviteConn, HitKind::Invite(false)),
                ("st-invite-full", ButtonText::InviteFull, HitKind::Invite(true)),
            );
            for index in 0..stand.guests.len() {
                lay.guest(index);
            }
        }
        Section::Link => {
            lay.choice("st-kill", "киллсвитч", Choice::Kill, HitKind::Kill);
            lay.field("st-dns", "dns", Focus::Dns, "свой резолвер", 240, false);
            if stand.kill != Kill::Off {
                lay.field("st-except", "исключения", Focus::Except, "dns через запятую", 240, false);
            }
            lay.choice("st-split", "маршруты", Choice::Split, HitKind::Split);
            lay.field("st-site", "сайт", Focus::Site, "домен", 240, false);
            for index in 0..stand.sites.len() {
                lay.entry(index, true);
            }
            lay.field("st-app", "приложение", Focus::App, "процесс", 240, false);
            for index in 0..stand.apps.len() {
                lay.entry(index, false);
            }
        }
    }
    if show_line {
        lay.rows.push(Row::Line { x: lay.x, y: lay.y });
        lay.y += 22.0;
    }
    Layout {
        fields: lay.fields,
        rows: lay.rows,
        hits: lay.hits,
        bottom: lay.y,
    }
}

fn nav_buttons(content: Rect) -> [NavBtn; 4] {
    const IDS: [&str; 4] = ["st-nav-server", "st-nav-proto", "st-nav-access", "st-nav-link"];
    const LABELS: [&str; 4] = ["сервер", "протоколы", "доступ", "связь"];
    const SECS: [Section; 4] = [Section::Server, Section::Protocols, Section::Access, Section::Link];
    let y = content.y + 6.0;
    let gap = 6.0;
    let x0 = content.x + 8.0;
    let inner = (content.width - 16.0 - gap * 3.0).max(0.0);
    let w = inner / 4.0;
    let mut out = [NavBtn {
        id: IDS[0],
        label: LABELS[0],
        section: Section::Server,
        rect: Rect::new(0.0, 0.0, 0.0, 0.0),
    }; 4];
    for i in 0..4 {
        out[i] = NavBtn {
            id: IDS[i],
            label: LABELS[i],
            section: SECS[i],
            rect: Rect::new(x0 + i as f64 * (w + gap), y, w.max(0.0), 30.0),
        };
    }
    out
}

fn qr_payload(stand: &Stand) -> Option<&str> {
    if stand.section != Section::Access {
        return None;
    }
    if let Some(index) = stand.qr_guest {
        if let Some(guest) = stand.guests.get(index) {
            if !guest.body.is_empty() {
                return Some(guest.body.as_str());
            }
        }
    }
    if stand.link.is_empty() {
        None
    } else {
        Some(stand.link.as_str())
    }
}

fn apply(stand: &mut Stand, kind: HitKind) {
    match kind {
        HitKind::Install => stand.install(),
        HitKind::Remove => stand.remove_remote(),
        HitKind::Export => stand.export(),
        HitKind::Copy => stand.copy_link(),
        HitKind::Scan => stand.request_scan(),
        HitKind::File => stand.import_file(),
        HitKind::Clip => stand.import_clipboard(),
        HitKind::Stack => stand.toggle_stack(),
        HitKind::Kill => stand.cycle_kill(),
        HitKind::Split => stand.toggle_split(),
        HitKind::Invite(full) => stand.invite(full),
        HitKind::Proto(index) => stand.select_proto(index),
        HitKind::ProtoOn(index) => stand.toggle_proto(index),
        HitKind::Guest(index) => stand.select_guest(index),
        HitKind::Revoke(index) => stand.revoke(index),
        HitKind::DropSite(index) => stand.drop_site(index),
        HitKind::DropApp(index) => stand.drop_app(index),
    }
}

pub fn register<S: uzor_framework::Spec>(
    coord: &mut InputCoordinator,
    view: &VisualView<S>,
    layer: &LayerId,
    at: impl Fn(Rect) -> Rect,
    g: &Geom,
    stand: &mut Stand,
) {
    for nav in nav_buttons(g.content) {
        coord.register_atomic(nav.id, WidgetKind::Button, at(nav.rect), Sense::CLICK, layer);
        if view.clicked(&WidgetId::from(nav.id)) {
            stand.set_section(nav.section);
        }
    }
    let lay = layout(stand, g.content, false);
    for hit in lay.hits {
        coord.register_atomic(hit.id.as_str(), WidgetKind::Button, at(hit.rect), Sense::CLICK, layer);
        if view.clicked(&WidgetId::from(hit.id.as_str())) {
            apply(stand, hit.kind);
        }
    }
}

/// Height of the content rect for the open section: nav, rows, status, QR.
pub fn content_height(stand: &Stand, note: bool) -> f64 {
    let show = note || !stand.status().is_empty();
    let lay = layout(stand, Rect::new(0.0, 0.0, 320.0, 1.0), show);
    let mut h = lay.bottom + 8.0;
    if qr_payload(stand).is_some() {
        h += 12.0 + 168.0;
    }
    h
}

pub fn fields(stand: &Stand, content: Rect) -> Vec<FieldSpec> {
    layout(stand, content, false).fields
}

pub fn paint(ctx: &mut dyn RenderContext, g: &Geom, stand: &Stand, _hot_sep: bool, note: Option<&str>) {
    ctx.set_fill_color("rgba(14,14,17,0.98)");
    ctx.fill_rect(g.panel.x, g.panel.y, g.panel.width, g.panel.height);
    ctx.set_fill_color("rgba(255,255,255,0.08)");
    ctx.fill_rect(0.0, g.tabbar.y + g.tabbar.height - 1.0, g.panel.width, 1.0);
    ctx.fill_rect(0.0, g.panel.y + g.panel.height - 1.0, g.panel.width, 1.0);

    paint_nav(ctx, g.content, stand.section);

    let status = stand.status();
    let note_line = note.unwrap_or("");
    let show = !status.is_empty() || !note_line.is_empty();
    let line = if !status.is_empty() { status } else { note_line.to_owned() };
    let lay = layout(stand, g.content, show);

    ctx.set_font("13px sans-serif");
    ctx.set_text_baseline(TextBaseline::Middle);
    for row in &lay.rows {
        paint_row(ctx, stand, row, &line);
    }
    if let Some(text) = qr_payload(stand) {
        paint_qr(ctx, g, text);
    }
}

fn paint_nav(ctx: &mut dyn RenderContext, content: Rect, section: Section) {
    ctx.set_font("11px sans-serif");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.set_line_dash(&[]);
    ctx.set_stroke_width(1.25);
    for nav in nav_buttons(content) {
        let color = if nav.section == section { AMBER } else { IDLE };
        ctx.set_stroke_color(color);
        ctx.stroke_rect(
            nav.rect.x + 0.5,
            nav.rect.y + 0.5,
            (nav.rect.width - 1.0).max(0.0),
            (nav.rect.height - 1.0).max(0.0),
        );
        ctx.set_fill_color(color);
        ctx.fill_text(nav.label, nav.rect.x + nav.rect.width / 2.0, nav.rect.y + nav.rect.height / 2.0);
    }
}

fn paint_row(ctx: &mut dyn RenderContext, stand: &Stand, row: &Row, line: &str) {
    ctx.set_font("13px sans-serif");
    ctx.set_text_baseline(TextBaseline::Middle);
    match row {
        Row::Field { rect, label, focus } => {
            plate(ctx, *rect, stand.focus == *focus);
            ctx.set_text_align(TextAlign::Left);
            ctx.set_fill_color(DIM);
            ctx.fill_text(label, rect.x + 8.0, rect.y + rect.height / 2.0);
        }
        Row::Button { rect, text } => {
            plate(ctx, *rect, false);
            ctx.set_text_align(TextAlign::Center);
            ctx.set_fill_color(AMBER);
            let label = button_label(stand, text);
            ctx.fill_text(label, rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
        }
        Row::Choice { rect, label, which } => {
            plate(ctx, *rect, false);
            ctx.set_text_align(TextAlign::Left);
            ctx.set_fill_color(DIM);
            ctx.fill_text(label, rect.x + 8.0, rect.y + rect.height / 2.0);
            ctx.set_fill_color(FG);
            let value = choice_value(stand, which);
            ctx.fill_text(&value, rect.x + LABEL_W, rect.y + rect.height / 2.0);
        }
        Row::Proto { rect, index, toggle } => {
            let Some(proto) = stand.protocols.get(*index) else {
                return;
            };
            let active = !stand.stack_auto && stand.stack == proto.id;
            plate(ctx, *rect, active);
            ctx.set_text_align(TextAlign::Left);
            ctx.set_fill_color(if active { AMBER } else { FG });
            ctx.fill_text(proto.id, rect.x + 8.0, rect.y + rect.height / 2.0);
            // The port this face is on — the thing the person is choosing.
            if !proto.port.trim().is_empty() {
                ctx.set_fill_color(DIM);
                let port = format!(":{}", proto.port.trim());
                ctx.fill_text(&port, rect.x + LABEL_W, rect.y + rect.height / 2.0);
            }
            ctx.set_text_align(TextAlign::Center);
            ctx.set_fill_color(if proto.on { AMBER } else { DIM });
            let word = if proto.on { "вкл" } else { "выкл" };
            ctx.fill_text(word, toggle.x + toggle.width / 2.0, toggle.y + toggle.height / 2.0);
        }
        Row::Note { rect, text } => {
            ctx.set_text_align(TextAlign::Left);
            ctx.set_fill_color(DIM);
            ctx.fill_text(
                &short(text, 96),
                rect.x + 8.0,
                rect.y + rect.height / 2.0,
            );
        }
        Row::Guest { rect, index, revoke } => {
            let Some(guest) = stand.guests.get(*index) else {
                return;
            };
            plate(ctx, *rect, stand.qr_guest == Some(*index));
            ctx.set_text_align(TextAlign::Left);
            ctx.set_fill_color(FG);
            ctx.fill_text(&short(&guest.name, 16), rect.x + 8.0, rect.y + rect.height / 2.0);
            ctx.set_fill_color(DIM);
            let kind = if guest.full { "полный" } else { "подключение" };
            ctx.fill_text(kind, rect.x + LABEL_W, rect.y + rect.height / 2.0);
            ctx.set_text_align(TextAlign::Center);
            ctx.set_fill_color(AMBER);
            ctx.fill_text("отозвать", revoke.x + revoke.width / 2.0, revoke.y + revoke.height / 2.0);
        }
        Row::Entry { rect, index, site } => {
            let text = if *site {
                stand.sites.get(*index).map(String::as_str)
            } else {
                stand.apps.get(*index).map(String::as_str)
            };
            let Some(text) = text else {
                return;
            };
            plate(ctx, *rect, false);
            ctx.set_text_align(TextAlign::Left);
            ctx.set_fill_color(FG);
            ctx.fill_text(&short(text, 24), rect.x + 8.0, rect.y + rect.height / 2.0);
            ctx.set_text_align(TextAlign::Right);
            ctx.set_fill_color(DIM);
            ctx.fill_text("убрать", rect.x + rect.width - 8.0, rect.y + rect.height / 2.0);
        }
        Row::Line { x, y } => {
            ctx.set_text_align(TextAlign::Left);
            ctx.set_fill_color(DIM);
            ctx.fill_text(line, *x, *y + 10.0);
        }
    }
}

fn plate(ctx: &mut dyn RenderContext, rect: Rect, hot: bool) {
    ctx.set_fill_color(if hot { "rgba(251,178,106,0.10)" } else { "rgba(255,255,255,0.05)" });
    ctx.fill_rect(rect.x, rect.y, rect.width, rect.height);
}

fn button_label(stand: &Stand, text: &ButtonText) -> &'static str {
    let running = stand.bus.running.load(Ordering::Relaxed);
    match text {
        ButtonText::Install => {
            if running {
                "ставится…"
            } else {
                "установить"
            }
        }
        ButtonText::Remove => {
            if running {
                "снимается…"
            } else {
                "снять"
            }
        }
        ButtonText::Export => "экспорт",
        ButtonText::Copy => "копировать",
        ButtonText::Scan => "сканер",
        ButtonText::File => "файл",
        ButtonText::Clip => "буфер",
        ButtonText::InviteConn => "подключение",
        ButtonText::InviteFull => "полный",
    }
}

fn choice_value(stand: &Stand, which: &Choice) -> String {
    match which {
        Choice::Stack => {
            if stand.stack_auto {
                "авто".into()
            } else if stand.stack.is_empty() {
                stand
                    .protocols
                    .first()
                    .map(|p| p.id.to_owned())
                    .unwrap_or_default()
            } else {
                stand.stack.clone()
            }
        }
        Choice::Kill => match stand.kill {
            Kill::Off => "выкл".into(),
            Kill::Soft => "мягкий".into(),
            Kill::Strict => "жёсткий".into(),
        },
        Choice::Split => {
            if stand.split_bypass {
                "все кроме".into()
            } else {
                "только эти".into()
            }
        }
    }
}

fn short(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        return text.to_owned();
    }
    let head: String = text.chars().take(n).collect();
    format!("{head}…")
}

fn paint_qr(ctx: &mut dyn RenderContext, g: &Geom, link: &str) {
    let Ok(code) = QrCode::new(link.as_bytes()) else {
        return;
    };
    let n = code.width();
    if n == 0 {
        return;
    }
    let colors = code.to_colors();
    let size = (g.content.width - 48.0).clamp(80.0, 168.0);
    let cell = size / n as f64;
    let ox = g.content.x + (g.content.width - cell * n as f64) / 2.0;
    let oy = g.sep.y - size - 12.0;
    if oy < g.content.y {
        return;
    }
    ctx.set_fill_color("#0C0C10");
    ctx.fill_rect(ox - 4.0, oy - 4.0, cell * n as f64 + 8.0, cell * n as f64 + 8.0);
    ctx.set_fill_color("#F4F4F5");
    for y in 0..n {
        for x in 0..n {
            if colors[y * n + x] == Color::Dark {
                ctx.fill_rect(ox + x as f64 * cell, oy + y as f64 * cell, cell.ceil(), cell.ceil());
            }
        }
    }
}
