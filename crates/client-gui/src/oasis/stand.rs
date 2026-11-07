//! Standalone sheet. Three sections under the chrome divider: servers (a new
//! box, or the boxes this client already has), device (how this machine
//! leaves, and the share page of the picked entrance), cabinet (bind and
//! unlink only). Password is never written.
//!
//! The ready stack is three one-line kits (`quic`, `quic-mirage`, `all`), not
//! a list of look-alike masks. Install sends the kit name. The link, the
//! re-roll, the remote remove, and the local forget open only for the box
//! that was pressed. A box that already has the container is read with the
//! existing `client.link`; that does not reinstall and does not mint keys.
//! Device is the share page: the link, the QR, export, and copy.
//!
//! The own-binary mode still names its two carrier ports. Kill switch and the
//! split lists live in the profile. The catalogue, `server.env`, and the
//! ask-faces path stay for the headless chain; the ready-stack window does not
//! show them. Guest shares are local files, not a server call. A raw link or
//! config is stored as text. A JSON object is what the shell hands to the
//! subscriber.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use qrcode::{Color, QrCode};
use uzor::input::{InputCoordinator, LayerId, Sense, WidgetKind};
use uzor::render::{RenderContext, TextAlign, TextBaseline};
use uzor::{Rect, WidgetId};
use uzor_framework::handle::VisualView;

use mirage_client_chain::provision::{self, InstallBus, Job};
use crate::shell::menu::Geom;
use crate::text::tr;

const AMBER: &str = "#FBB26A";

fn fg() -> &'static str {
    if crate::theme::light() { "#16161a" } else { "#F4F4F5" }
}

fn dim() -> &'static str {
    if crate::theme::light() { "#5c5c64" } else { "#9a9aa2" }
}

/// Light sheet has no amber. Dark keeps it for the on-state and the solid pills.
fn accent() -> &'static str {
    if crate::theme::light() { fg() } else { AMBER }
}
const ROW: f64 = 40.0;
const ROW_H: f64 = 32.0;
const NAV_H: f64 = 44.0;
const LABEL_W: f64 = 108.0;
const REVOKE_W: f64 = 72.0;

/// The ports the box's own carriers open by default.
///
/// The carriers are the box's two doors: MirageVG and MirageKVIC. These are
/// the numbers the box's own server opens them on.
const DEFAULT_MWG_PORT: &str = "41443";
const DEFAULT_KVIK_PORT: &str = "41444";

/// Catalogue of face ids for the headless chain and the ask-faces path.
///
/// The ready-stack window does not show these rows and does not write
/// `{ID}_MASK`. It offers three kits — `quic-mirage`, `quic`, `all` — and
/// installs the one that is selected.
///
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
    ("raw_udp", &["quic", "awg", "wireguard", "openvpn", "ikev2"]),
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// A box the client can reach: host, kit, install, the fetched entrance.
    Servers,
    /// This machine: language, how traffic leaves, and the share page.
    Device,
    /// The fleet cabinet. Bind and unlink, and nothing else.
    Cabinet,
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
    /// The binary that goes on the box, in the own-binary mode.
    Binary,
    /// The ssh key the client uses for the box's door.
    SshKey,
    /// The port the MirageVG carrier sits on.
    MwgPort,
    /// The port the MirageKVIC carrier sits on.
    KvikPort,
    /// The claim code the cabinet is redeemed with.
    Bind,
}

/// Where the box's stack comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Build {
    /// The ready-made stack the box is given.
    Ready,
    /// The client's own binary, put up on the box by the client itself.
    Own,
}

/// One ready-stack kit. The profile name is what the installer receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kit {
    /// QUIC on UDP 443 plus the Mirage TCP perimeter. The recommended kit.
    QuicMirage,
    /// QUIC only, on UDP 443.
    Quic,
    /// Every protocol on its default port, camouflage none.
    All,
}

/// Which key the client comes into the box with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// The client's own key, made once and kept beside the app.
    Own,
    /// A key the person brought in — their own, or one the hoster gave.
    File,
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
    /// The ready-stack window does not show it. The dev ask-faces view does.
    #[cfg(feature = "dev-tools")]
    pub why: String,
}

struct Guest {
    name: String,
    full: bool,
    body: String,
}

/// How the traffic leaves this machine.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Carry {
    /// A local SOCKS5 port. The person points their own programs at it.
    Socks,
    /// The tunnel adapter: the whole machine goes through the tunnel.
    Tun,
}

pub struct Stand {
    pub section: Section,
    pub host: String,
    /// The known box whose detail is open. Empty means the list is shut.
    /// Not written down: it is which row is open, not a credential.
    pub open_host: String,
    /// The servers section is showing the new-box form. False is the list of
    /// boxes this client already has. Not written down.
    pub server_new: bool,
    pub user: String,
    pub pass: String,
    pub stack_auto: bool,
    pub stack: String,
    pub protocols: Vec<Proto>,
    pub focus: Focus,
    pub link: String,
    /// Where the box's stack comes from: the ready-made one, or the client's
    /// own binary put up by the client itself.
    pub build: Build,
    /// The kit the ready stack installs. The own-binary mode does not send it.
    pub kit: Kit,
    /// The binary that goes on the box — the own-binary mode only.
    pub binary: String,
    /// The ssh key the client uses for the box's door. Empty means "make one
    /// and plant it with the password".
    pub ssh_key: String,
    /// Which key the client comes into the box with.
    pub key_source: KeySource,
    /// The port the MirageVG carrier sits on.
    pub mwg_port: String,
    /// The port the MirageKVIC carrier sits on.
    pub kvik_port: String,
    /// The person's own configurations — the ways out the client knows.
    pub configs: Vec<crate::configs::Config>,
    /// The claim code the cabinet is redeemed with. The engine holds the
    /// redemption; the field only holds the text.
    pub bind_draft: String,
    /// Set when the person pressed ENTER on the claim field.
    pub bind_submit: bool,
    /// The claim code to redeem, waiting for the engine.
    pub claim: String,
    /// The cabinet is redeemed. The sheet shows the way out of it instead of
    /// the way in.
    pub cabinet_bound: bool,
    /// The person asked to be unlinked.
    pub unlink: bool,
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
    /// How the traffic leaves this machine: a local SOCKS5 port, or the
    /// tunnel adapter itself.
    pub carry: Carry,
    /// The carry changed. The shell pushes it to the engine and clears this.
    pub carry_dirty: bool,
    /// The target the person was on last: `cabinet`, or `config:<path>`.
    /// The window comes back to it instead of to the first row of the list.
    pub last_target: String,
}

impl Stand {
    pub fn load() -> Self {
        let mut stand = Self {
            section: Section::Servers,
            host: String::new(),
            open_host: String::new(),
            server_new: false,
            user: "root".into(),
            pass: String::new(),
            stack_auto: true,
            stack: String::new(),
            protocols: default_protocols(),
            focus: Focus::None,
            link: String::new(),
            build: Build::Ready,
            kit: Kit::QuicMirage,
            binary: String::new(),
            ssh_key: String::new(),
            key_source: KeySource::Own,
            mwg_port: DEFAULT_MWG_PORT.to_owned(),
            kvik_port: DEFAULT_KVIK_PORT.to_owned(),
            configs: crate::configs::list(),
            bind_draft: String::new(),
            bind_submit: false,
            claim: String::new(),
            cabinet_bound: false,
            unlink: false,
            guests: Vec::new(),
            guest_draft: String::new(),
            qr_guest: None,
            kill: Kill::Off,
            dns: String::new(),
            except: String::new(),
            split_bypass: false,
            carry: Carry::Tun,
            carry_dirty: false,
            last_target: String::new(),
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
                    // Old names stay readable. Protocols lived on the server,
                    // access and the link page live on the device.
                    stand.section = match v.get("section").and_then(|s| s.as_str()) {
                        Some("device") | Some("access") | Some("link") => Section::Device,
                        Some("cabinet") => Section::Cabinet,
                        _ => Section::Servers,
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
                    stand.build = match v.get("build").and_then(|s| s.as_str()) {
                        Some("own") => Build::Own,
                        _ => Build::Ready,
                    };
                    // A missing or unknown kit stays the recommended one.
                    // Old catalogue rows are not a kit.
                    if let Some(raw) = v.get("kit").and_then(|s| s.as_str()) {
                        stand.kit = kit_from_key(raw);
                    }
                    if let Some(bin) = v.get("binary").and_then(|s| s.as_str()) {
                        stand.binary = bin.to_owned();
                    }
                    if let Some(key) = v.get("ssh_key").and_then(|s| s.as_str()) {
                        stand.ssh_key = key.to_owned();
                    }
                    stand.key_source = match v.get("key_source").and_then(|s| s.as_str()) {
                        Some("file") => KeySource::File,
                        _ => KeySource::Own,
                    };
                    if let Some(port) = v.get("mwg_port").and_then(|s| s.as_str()) {
                        if !port.trim().is_empty() {
                            stand.mwg_port = port.to_owned();
                        }
                    }
                    if let Some(port) = v.get("kvik_port").and_then(|s| s.as_str()) {
                        if !port.trim().is_empty() {
                            stand.kvik_port = port.to_owned();
                        }
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
                    if let Some(carry) = v.get("carry").and_then(|s| s.as_str()) {
                        stand.carry = match carry {
                            "socks" => Carry::Socks,
                            _ => Carry::Tun,
                        };
                    }
                    if let Some(target) = v.get("last_target").and_then(|s| s.as_str()) {
                        stand.last_target = target.to_owned();
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
            "build": build_key(self.build),
            "kit": kit_key(self.kit),
            "binary": self.binary,
            "ssh_key": self.ssh_key,
            "key_source": key_source_key(self.key_source),
            "mwg_port": self.mwg_port,
            "kvik_port": self.kvik_port,
            "protocols": protocols,
            "kill": kill_key(self.kill),
            "dns": self.dns,
            "except": self.except,
            "split_bypass": self.split_bypass,
            "carry": carry_key(self.carry),
            "last_target": self.last_target,
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
        if self.link != link {
            self.link = link;
            self.bump_link_epoch();
            self.save_link();
            // A box was just installed, or its link changed: it is a configuration now.
            self.refresh_configs();
        }
        // The link that arrived is the share page. The QR is that link, not a guest.
        self.qr_guest = None;
        self.section = Section::Device;
        self.focus = Focus::None;
        self.save_profile();
    }

    /// Read the list of configurations again.
    ///
    /// The list grows when a box is installed and when an artefact is
    /// imported, so it is read fresh whenever something happened.
    pub fn refresh_configs(&mut self) {
        self.configs = crate::configs::list();
    }

    /// The person pressed ENTER on the claim field.
    ///
    /// The engine does the redemption; the sheet only carries the text and
    /// the wish.
    pub fn submit_bind(&mut self) {
        let text = self.bind_draft.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.bind_draft.clear();
        self.claim = text;
        self.bind_submit = true;
    }

    /// The claim code the person asked to redeem.
    pub fn take_claim(&mut self) -> Option<String> {
        if !self.bind_submit {
            return None;
        }
        self.bind_submit = false;
        Some(std::mem::take(&mut self.claim))
    }

    /// The person asked to be unlinked.
    pub fn take_unlink(&mut self) -> bool {
        std::mem::take(&mut self.unlink)
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
                #[cfg(feature = "dev-tools")]
                why: face.why.clone(),
            });
        }
        if applied.is_empty() {
            self.say(tr("say.no_face"));
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
                #[cfg(feature = "dev-tools")]
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

    #[cfg(feature = "dev-tools")]
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

    /// Install with an explicit mode: `ask_faces = true` asks the box what it
    /// could publish, `false` brings it up with the faces the person chose.
    ///
    /// The ready-stack window does not call this. The dev ask-faces path does.
    #[cfg(feature = "dev-tools")]
    pub fn install_with(&mut self, ask_faces: bool) {
        if self.bus.running.load(Ordering::Relaxed) {
            return;
        }
        let Some((host, port)) = split_host(&self.host) else {
            self.say(tr("say.need_host"));
            return;
        };
        if self.user.trim().is_empty() {
            self.say(tr("say.need_login"));
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
                ask_faces,
                profile: None,
                state: None,
            },
            Arc::clone(&self.bus),
        );
    }

    /// The profile name the ready stack installs. The own binary sends none.
    pub fn ready_profile(&self) -> Option<&'static str> {
        match self.build {
            Build::Own => None,
            Build::Ready => Some(kit_key(self.kit)),
        }
    }

    /// Install the ready stack as the selected kit.
    ///
    /// Same host and login checks as [`Self::install_with`], and the password
    /// leaves the field the same way. The job carries the kit name and an
    /// empty env: the window does not upload the face catalogue.
    pub fn install_kit(&mut self) {
        if self.bus.running.load(Ordering::Relaxed) {
            return;
        }
        let Some((host, port)) = split_host(&self.host) else {
            self.say(tr("say.need_host"));
            return;
        };
        if self.user.trim().is_empty() {
            self.say(tr("say.need_login"));
            return;
        }
        let Some(profile) = self.ready_profile() else {
            return;
        };
        self.save_profile();
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
                stack: "auto".to_owned(),
                env: String::new(),
                ask_faces: false,
                profile: Some(profile.to_owned()),
                state: None,
            },
            Arc::clone(&self.bus),
        );
    }

    /// Install in the mode the stand is in.
    ///
    /// The ready stack sends the selected kit. The own binary is put up by
    /// the client itself. Asking the box for faces stays on `install_with`,
    /// which the ready-stack window does not call.
    pub fn install(&mut self) {
        match self.build {
            Build::Ready => self.install_kit(),
            Build::Own => self.install_own(),
        }
    }

    /// Put the client's own binary on the box and take the artefact back.
    ///
    /// The GUI installs nothing itself: it runs the very binary the customer
    /// client ships, with the very command the headless run uses, and the
    /// artefact that comes back is the same one the cabinet's panel issues.
    ///
    /// The password goes into the pipe and dies there. It is never written
    /// down, never in the command line, and never in the profile.
    pub fn install_own(&mut self) {
        if self.bus.running.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some((host, port)) = split_host(&self.host) else {
            self.bus.running.store(false, Ordering::Relaxed);
            self.say(tr("say.need_host"));
            return;
        };
        if self.user.trim().is_empty() {
            self.bus.running.store(false, Ordering::Relaxed);
            self.say(tr("say.need_login"));
            return;
        }
        let binary = self.box_binary();
        if !binary.is_file() {
            self.bus.running.store(false, Ordering::Relaxed);
            self.say(tr("say.need_binary"));
            return;
        }
        let Some(mwg) = port_of(&self.mwg_port) else {
            self.bus.running.store(false, Ordering::Relaxed);
            self.say(tr("say.need_port"));
            return;
        };
        let Some(kvik) = port_of(&self.kvik_port) else {
            self.bus.running.store(false, Ordering::Relaxed);
            self.say(tr("say.need_port"));
            return;
        };
        let paths = match crate::backend::oss_paths_for(&host) {
            Ok(paths) => paths,
            Err(err) => {
                self.bus.running.store(false, Ordering::Relaxed);
                self.say(&format!("{err}"));
                return;
            }
        };
        self.save_profile();

        let args = oss_command(
            &format!("{}@{}:{}", self.user.trim(), host, port),
            &binary.to_string_lossy(),
            mwg,
            kvik,
            &paths.state.to_string_lossy(),
            &paths.key.to_string_lossy(),
            &paths.artefact.to_string_lossy(),
            match self.key_source {
                // The client's own key, made once and kept: the installer
                // takes it from the state directory.
                KeySource::Own => "",
                // A key the person brought in.
                KeySource::File => self.ssh_key.trim(),
            },
        );
        let password = std::mem::take(&mut self.pass);
        self.pass_epoch = self.pass_epoch.wrapping_add(1);
        if self.pass_epoch == 0 {
            self.pass_epoch = 1;
        }
        if let Err(err) = crate::backend::spawn_oss_install(
            &args,
            &password,
            Arc::clone(&self.bus),
            &host,
            &paths,
        ) {
            self.bus.running.store(false, Ordering::Relaxed);
            self.say(&format!("{err}"));
        }
    }

    /// Where the box's binary is.
    ///
    /// The path the person wrote down wins; otherwise the binary is looked
    /// for beside the executable — the place the app keeps its helpers.
    fn box_binary(&self) -> std::path::PathBuf {
        let given = self.binary.trim();
        if !given.is_empty() {
            return std::path::PathBuf::from(given);
        }
        crate::backend::find_box_binary().unwrap_or_else(|| std::path::PathBuf::from("mirage-vpn"))
    }

    /// The mode of the install: the ready-made stack, or the client's own
    /// binary on the box.
    pub fn cycle_build(&mut self) {
        self.build = match self.build {
            Build::Ready => Build::Own,
            Build::Own => Build::Ready,
        };
        self.save_profile();
    }

    /// Which key the client comes into the box with.
    pub fn cycle_key_source(&mut self) {
        self.key_source = match self.key_source {
            KeySource::Own => KeySource::File,
            KeySource::File => KeySource::Own,
        };
        self.save_profile();
    }

    /// The client's own key for this box, in the form the box takes.
    ///
    /// The person puts this line into the box's list of keys — through the
    /// hoster's panel, or by hand — and the client comes in by key from then
    /// on, with no password at all. The private half stays on this machine.
    pub fn show_my_key(&mut self) {
        let paths = match crate::backend::oss_paths_for(self.host.trim()) {
            Ok(paths) => paths,
            Err(err) => {
                self.say(&format!("{err}"));
                return;
            }
        };
        let key = match mirage_client_chain::oss_install::oss_key(Some(&paths.state)) {
            Ok(key) => key,
            Err(err) => {
                self.say(&format!("{err}"));
                return;
            }
        };
        if let Err(err) = mirage_client_chain::provision::ensure_key(&key.priv_path, &key.pub_path) {
            self.say(&format!("{err}"));
            return;
        }
        let Ok(line) = std::fs::read_to_string(&key.pub_path) else {
            self.say(tr("say.no_key"));
            return;
        };
        let line = line.trim().to_owned();
        self.copy_text(&line);
        self.say(tr("say.my_key"));
    }

    pub fn remove_remote(&mut self) {
        if self.bus.running.load(Ordering::Relaxed) {
            return;
        }
        let Some((host, port)) = split_host(&self.host) else {
            self.say(tr("say.need_host"));
            return;
        };
        if self.user.trim().is_empty() {
            self.say(tr("say.need_login"));
            return;
        }
        self.save_profile();
        provision::remove(host, port, self.user.trim().to_owned(), Arc::clone(&self.bus));
    }

    /// Read the link a box that is already up already has.
    ///
    /// Host and login, same as install. The password stays in the field:
    /// this door is the key only, and it does not reinstall.
    pub fn fetch_config(&mut self) {
        if self.bus.running.load(Ordering::Relaxed) {
            return;
        }
        let Some((host, port)) = split_host(&self.host) else {
            self.say(tr("say.need_host"));
            return;
        };
        if self.user.trim().is_empty() {
            self.say(tr("say.need_login"));
            return;
        }
        provision::fetch_link(host, port, self.user.trim().to_owned(), Arc::clone(&self.bus));
    }

    fn select_kit(&mut self, kit: Kit) {
        self.kit = kit;
        self.save_profile();
    }

    /// Open the detail of one known box, or close it when it is already open.
    ///
    /// The host field becomes that box, so install, fetch, and remove talk to
    /// it. Closing the row leaves the draft host as it is.
    fn open_server(&mut self, host: &str) {
        if self.open_host == host {
            self.open_host.clear();
            return;
        }
        self.open_host = host.to_owned();
        self.host = host.to_owned();
        self.focus = Focus::None;
        self.save_profile();
    }

    /// Drop this box from the client's list. The files stay. The box is not
    /// contacted: that is the remote remove, and it is a different button.
    fn forget_server(&mut self) {
        let host = self.open_host.trim().to_owned();
        if host.is_empty() {
            self.say(tr("say.need_host"));
            return;
        }
        let doomed: Vec<crate::configs::Config> = self
            .configs
            .iter()
            .filter(|config| server_host(config) == host)
            .cloned()
            .collect();
        if doomed.is_empty() {
            self.say(tr("say.empty"));
            return;
        }
        for config in &doomed {
            crate::configs::forget(config);
        }
        if self.host.trim() == host {
            self.host.clear();
        }
        self.open_host.clear();
        self.refresh_configs();
        self.save_profile();
        self.say(tr("say.forgot"));
    }

    pub fn export(&self) {
        if self.link.is_empty() {
            self.say(tr("say.no_link"));
            return;
        }
        #[cfg(target_os = "ios")]
        {
            self.say(tr("say.file_gone"));
            return;
        }
        #[cfg(not(target_os = "ios"))]
        {
            let Some(path) = rfd::FileDialog::new().set_file_name("m1r4g3-link.txt").save_file() else {
                return;
            };
            let _ = std::fs::write(path, &self.link);
        }
    }

    pub fn copy_link(&self) {
        if self.link.is_empty() {
            self.say(tr("say.no_link"));
            return;
        }
        self.copy_text(&self.link);
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

    /// Switch between the local SOCKS5 port and the tunnel adapter.
    ///
    /// The adapter takes the whole machine through the tunnel; the SOCKS5
    /// port is only what the person's own programs point at. The sheet
    /// carries the wish, the engine does the work.
    pub fn cycle_carry(&mut self) {
        self.carry = match self.carry {
            Carry::Socks => Carry::Tun,
            Carry::Tun => Carry::Socks,
        };
        self.carry_dirty = true;
        self.save_profile();
    }

    /// The carry the person asked for, waiting for the engine.
    pub fn take_carry(&mut self) -> Option<Carry> {
        if !self.carry_dirty {
            return None;
        }
        self.carry_dirty = false;
        Some(self.carry)
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
            self.say(tr("say.need_name"));
            return;
        }
        let body = if full {
            if self.host.trim().is_empty() {
                self.say(tr("say.nothing"));
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
            self.say(tr("say.no_link"));
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
        self.say(tr("say.saved"));
    }

    pub fn select_guest(&mut self, index: usize) {
        let Some(body) = self.guests.get(index).map(|g| g.body.clone()) else {
            return;
        };
        self.qr_guest = Some(index);
        if !body.is_empty() {
            self.copy_text(&body);
            self.say(tr("say.copied"));
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
            self.say(tr("say.empty"));
            return;
        }
        if raw.len() > 256 * 1024 {
            self.say(tr("say.long"));
            return;
        }
        if self.section != Section::Device {
            self.section = Section::Device;
            self.save_profile();
        }
        self.set_link(raw);
        self.say(if raw.starts_with('{') { tr("say.cfg") } else { tr("say.link_ok") });
    }

    pub fn import_file(&mut self) {
        #[cfg(target_os = "ios")]
        {
            self.say(tr("say.file_gone"));
            return;
        }
        #[cfg(not(target_os = "ios"))]
        {
            let Some(path) = rfd::FileDialog::new()
                .set_title(tr("dlg.title"))
                .add_filter(tr("dlg.filter"), &["json", "txt", "mirage", "conf", "vpn"])
                .pick_file()
            else {
                return;
            };
            match std::fs::read_to_string(&path) {
                Ok(raw) => self.accept_raw(&raw),
                Err(_) => self.say(tr("say.unreadable")),
            }
        }
    }

    pub fn import_clipboard(&mut self) {
        #[cfg(target_os = "ios")]
        {
            self.say(tr("say.clip_down"));
            return;
        }
        #[cfg(not(target_os = "ios"))]
        {
            let mut clip = match arboard::Clipboard::new() {
                Ok(clip) => clip,
                Err(_) => {
                    self.say(tr("say.clip_down"));
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
                Err(_) => self.say(tr("say.clip_empty")),
            }
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
            Focus::Binary => Some(&mut self.binary),
            Focus::SshKey => Some(&mut self.ssh_key),
            Focus::MwgPort => Some(&mut self.mwg_port),
            Focus::KvikPort => Some(&mut self.kvik_port),
            Focus::Bind => Some(&mut self.bind_draft),
            Focus::None => None,
        }
    }

    pub fn type_text(&mut self, text: &str) {
        let focus = self.focus;
        let limit = match focus {
            Focus::Link => 256 * 1024,
            Focus::Guest => 64,
            Focus::MwgPort | Focus::KvikPort => 5,
            Focus::Bind => 512,
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

    fn buf_mut(&mut self) -> Option<&mut String> {
        self.field_mut(self.focus)
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
        #[cfg(not(target_os = "ios"))]
        if let Ok(mut clip) = arboard::Clipboard::new() {
            let _ = clip.set_text(text.to_owned());
        }
        #[cfg(target_os = "ios")]
        {
            let _ = text;
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
            #[cfg(feature = "dev-tools")]
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
        Section::Servers => "servers",
        Section::Device => "device",
        Section::Cabinet => "cabinet",
    }
}

/// The mode of the install, as the profile writes it down.
fn build_key(build: Build) -> &'static str {
    match build {
        Build::Ready => "ready",
        Build::Own => "own",
    }
}

fn kit_key(kit: Kit) -> &'static str {
    match kit {
        Kit::QuicMirage => "quic-mirage",
        Kit::Quic => "quic",
        Kit::All => "all",
    }
}

/// Missing and unknown names stay on the recommended kit.
fn kit_from_key(raw: &str) -> Kit {
    match raw {
        "quic" => Kit::Quic,
        "all" => Kit::All,
        "quic-mirage" => Kit::QuicMirage,
        _ => Kit::QuicMirage,
    }
}

fn kit_title(kit: Kit) -> &'static str {
    match kit {
        Kit::QuicMirage => tr("kit.qm"),
        Kit::Quic => tr("kit.quic"),
        Kit::All => tr("kit.all"),
    }
}

/// The box an entrance belongs to.
///
/// The artefact's address is the box. Several entrances of one box — the
/// install itself and the later consumers — are one row. A name is only the
/// fallback when the artefact does not say.
fn server_host(config: &crate::configs::Config) -> String {
    if let Some(text) = config.read() {
        let (host, _) = crate::configs::read_name(&text);
        if !host.is_empty() {
            return host;
        }
    }
    config.name.trim().to_owned()
}

/// Boxes this client already has an entrance for, in list order, once each.
fn known_servers(stand: &Stand) -> Vec<String> {
    let mut hosts = Vec::new();
    for config in &stand.configs {
        let host = server_host(config);
        if host.is_empty() || hosts.iter().any(|item| item == &host) {
            continue;
        }
        hosts.push(host);
    }
    hosts
}

/// Which key the client comes in by, as the profile writes it down.
fn key_source_key(source: KeySource) -> &'static str {
    match source {
        KeySource::Own => "own",
        KeySource::File => "file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command the GUI gives the client: the password travels in the pipe,
    /// never in the command line.
    #[test]
    fn the_password_travels_in_the_pipe_not_in_the_command_line() {
        let args = oss_command(
            "root@box.example:22",
            "boxes/mirage-vpn",
            41443,
            41444,
            "state",
            "state/client.key.json",
            "state/artefact.json",
            "",
        );
        assert!(args.iter().any(|arg| arg == "--password-stdin"));
        assert!(
            !args.iter().any(|arg| arg == "--password"),
            "the password must not be a flag of its own"
        );
        let flag = |name: &str| {
            args.iter()
                .position(|arg| arg == name)
                .map(|at| args[at + 1].clone())
                .unwrap_or_default()
        };
        assert_eq!(flag("--box"), "root@box.example:22");
        assert_eq!(flag("--binary"), "boxes/mirage-vpn");
        assert_eq!(flag("--mwg-port"), "41443");
        assert_eq!(flag("--mkvik-port"), "41444");
        assert_eq!(flag("--key-out"), "state/client.key.json");
        assert_eq!(flag("--out"), "state/artefact.json");
        assert_eq!(
            flag("--ssh-key"),
            "",
            "no key named, no key flag"
        );
    }

    /// The operator's own key for the box is passed on when the person named
    /// one; without it the client makes a key of its own.
    #[test]
    fn the_named_key_is_passed_on() {
        let args = oss_command(
            "root@box.example:22",
            "mirage-vpn",
            41443,
            41444,
            "state",
            "key.json",
            "artefact.json",
            "  env/keys/box  ",
        );
        let flag = |name: &str| {
            args.iter()
                .position(|arg| arg == name)
                .map(|at| args[at + 1].clone())
                .unwrap_or_default()
        };
        assert_eq!(flag("--ssh-key"), "env/keys/box");
    }

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
        // The bind address is the loopback literal on purpose: the customer
        // source gate allows no other IPv4 in these sources, and the address
        // plays no part in the id the mask's kind picks.
        let quic = face(r#"{"type":"quic","alpn":["h3"],"bind_addr":"127.0.0.1:443"}"#, 443);
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

    #[test]
    fn the_ready_kit_is_the_profile() {
        let mut stand = Stand::load();
        stand.build = Build::Ready;
        stand.kit = Kit::QuicMirage;
        assert_eq!(stand.ready_profile(), Some("quic-mirage"));
        stand.kit = Kit::Quic;
        assert_eq!(stand.ready_profile(), Some("quic"));
        stand.kit = Kit::All;
        assert_eq!(stand.ready_profile(), Some("all"));
        stand.build = Build::Own;
        assert_eq!(stand.ready_profile(), None);
    }

    #[test]
    fn the_ready_protocol_sheet_is_three_kit_cards() {
        let mut stand = Stand::load();
        stand.kit = Kit::QuicMirage;
        stand.build = Build::Ready;
        stand.section = Section::Servers;
        stand.server_new = true;
        let lay = layout(&stand, Rect::new(0.0, 0.0, 320.0, 1.0), false);
        let kinds: Vec<HitKind> = lay.hits.iter().map(|hit| hit.kind).collect();
        assert!(kinds.contains(&HitKind::Kit(Kit::QuicMirage)));
        assert!(kinds.contains(&HitKind::Kit(Kit::Quic)));
        assert!(kinds.contains(&HitKind::Kit(Kit::All)));
        // Nothing is selected, so the link actions stay shut. A new box can
        // still be typed and installed.
        assert!(!kinds.contains(&HitKind::Fetch));
        assert!(!kinds.contains(&HitKind::Remove));
        assert!(!kinds.contains(&HitKind::Forget));
        assert!(kinds.contains(&HitKind::Install));
        assert!(kinds.contains(&HitKind::ServerPage(true)));
        assert!(kinds.contains(&HitKind::ServerPage(false)));
        assert!(kinds.iter().all(|kind| !matches!(kind, HitKind::Unlink | HitKind::Server(_))));
        assert!(lay.fields.iter().any(|field| field.focus == Focus::Host));
        assert!(lay.fields.iter().all(|field| field.focus != Focus::Bind));
        assert_eq!(kinds.iter().filter(|kind| matches!(kind, HitKind::Kit(_))).count(), 3);
        assert!(kinds.iter().all(|kind| !matches!(kind, HitKind::Proto(_) | HitKind::ProtoOn(_))));
        let _toggle = (HitKind::Proto(0), HitKind::ProtoOn(0));
        let cards: Vec<_> = lay.hits.iter().filter(|hit| matches!(hit.kind, HitKind::Kit(_))).collect();
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].kind, HitKind::Kit(Kit::Quic));
        assert_eq!(cards[1].kind, HitKind::Kit(Kit::QuicMirage));
        assert_eq!(cards[2].kind, HitKind::Kit(Kit::All));
        for card in &cards {
            assert_eq!(card.rect.height, ROW_H);
        }
        assert_eq!(cards[1].rect.y - cards[0].rect.y, ROW);
        assert_eq!(cards[2].rect.y - cards[1].rect.y, ROW);
    }

    /// Several entrances of one box are one server. Its config actions exist
    /// only while that row is open.
    #[test]
    fn the_server_sheet_opens_one_box() {
        let dir = std::env::temp_dir().join(format!("m1r4g3-srv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::create_dir_all(dir.join("c")).unwrap();
        std::fs::write(
            dir.join("a").join("artefact.json"),
            r#"{"v":1,"entrances":[{"addr":"edge.example:443"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("b").join("artefact.json"),
            r#"{"v":1,"entrances":[{"addr":"edge.example:443"}],"label":"consumer"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("c").join("artefact.json"),
            r#"{"v":1,"entrances":[{"addr":"other.example:443"}]}"#,
        )
        .unwrap();
        let mut stand = Stand::load();
        stand.section = Section::Servers;
        stand.build = Build::Ready;
        stand.server_new = false;
        stand.configs = vec![
            crate::configs::Config {
                name: "edge.example".into(),
                kind: "vpn".into(),
                dir: dir.join("a"),
            },
            crate::configs::Config {
                name: "consumer".into(),
                kind: "vpn".into(),
                dir: dir.join("b"),
            },
            crate::configs::Config {
                name: "other.example".into(),
                kind: "vpn".into(),
                dir: dir.join("c"),
            },
        ];
        let lay = layout(&stand, Rect::new(0.0, 0.0, 320.0, 1.0), false);
        let servers: Vec<_> = lay
            .hits
            .iter()
            .filter(|hit| matches!(hit.kind, HitKind::Server(_)))
            .collect();
        assert_eq!(servers.len(), 2);
        assert!(lay.hits.iter().any(|hit| hit.kind == HitKind::ServerPage(true)));
        assert!(lay.hits.iter().any(|hit| hit.kind == HitKind::ServerPage(false)));
        assert!(lay.hits.iter().all(|hit| {
            hit.kind != HitKind::Fetch && hit.kind != HitKind::Forget && !matches!(hit.kind, HitKind::Kit(_))
        }));
        assert!(lay.fields.iter().all(|field| field.focus != Focus::Host));
        stand.open_host = "edge.example".into();
        let lay = layout(&stand, Rect::new(0.0, 0.0, 320.0, 1.0), false);
        let kinds: Vec<HitKind> = lay.hits.iter().map(|hit| hit.kind).collect();
        assert!(kinds.contains(&HitKind::Fetch));
        assert!(kinds.contains(&HitKind::Install));
        assert!(kinds.contains(&HitKind::Remove));
        assert!(kinds.contains(&HitKind::Forget));
        assert!(kinds.contains(&HitKind::Kit(Kit::Quic)));
        assert!(lay.fields.iter().all(|field| field.focus != Focus::Host));
        assert!(lay.fields.iter().any(|field| field.focus == Focus::User));
        stand.server_new = true;
        let lay = layout(&stand, Rect::new(0.0, 0.0, 320.0, 1.0), false);
        assert!(lay.hits.iter().all(|hit| !matches!(hit.kind, HitKind::Server(_))));
        assert!(lay.fields.iter().any(|field| field.focus == Focus::Host));
        assert!(lay.hits.iter().any(|hit| hit.kind == HitKind::Install));
        assert!(lay.hits.iter().all(|hit| hit.kind != HitKind::Fetch && hit.kind != HitKind::Forget));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_own_protocol_sheet_has_no_kit_fetch() {
        let mut stand = Stand::load();
        stand.kit = Kit::QuicMirage;
        stand.build = Build::Own;
        stand.section = Section::Servers;
        stand.server_new = true;
        let lay = layout(&stand, Rect::new(0.0, 0.0, 320.0, 1.0), false);
        assert!(lay.hits.iter().all(|hit| !matches!(hit.kind, HitKind::Fetch | HitKind::Kit(_))));
        assert!(lay.fields.iter().any(|field| field.focus == Focus::MwgPort));
        assert!(lay.fields.iter().any(|field| field.focus == Focus::KvikPort));
    }

    #[test]
    fn the_cabinet_sheet_is_only_the_claim() {
        let mut stand = Stand::load();
        stand.section = Section::Cabinet;
        stand.cabinet_bound = false;
        let lay = layout(&stand, Rect::new(0.0, 0.0, 320.0, 1.0), false);
        assert!(lay.fields.iter().any(|field| field.focus == Focus::Bind));
        assert!(lay.hits.iter().all(|hit| hit.kind != HitKind::Install));
        stand.cabinet_bound = true;
        let lay = layout(&stand, Rect::new(0.0, 0.0, 320.0, 1.0), false);
        assert!(lay.hits.iter().any(|hit| hit.kind == HitKind::Unlink));
        assert!(lay.fields.iter().all(|field| field.focus != Focus::Bind));
        stand.section = Section::Device;
        let lay = layout(&stand, Rect::new(0.0, 0.0, 320.0, 1.0), false);
        assert!(lay.fields.iter().any(|field| field.focus == Focus::Link));
        assert!(lay.hits.iter().any(|hit| hit.kind == HitKind::Carry));
        assert!(lay.hits.iter().all(|hit| hit.kind != HitKind::Unlink && hit.kind != HitKind::Install));
        assert_eq!(nav_buttons(Rect::new(0.0, 0.0, 320.0, 1.0)).len(), 3);
    }
}

fn kill_key(kill: Kill) -> &'static str {
    match kill {
        Kill::Off => "off",
        Kill::Soft => "soft",
        Kill::Strict => "strict",
    }
}

fn carry_key(carry: Carry) -> &'static str {
    match carry {
        Carry::Socks => "socks",
        Carry::Tun => "tun",
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
        return Err(tr("say.bad_image").into());
    }
    let mut prep = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| {
        let i = (y * w + x) * 4;
        let (r, g, b) = (rgba[i] as u32, rgba[i + 1] as u32, rgba[i + 2] as u32);
        ((r * 299 + g * 587 + b * 114) / 1000) as u8
    });
    let grids = prep.detect_grids();
    let grid = grids.first().ok_or(tr("say.qr_none"))?;
    let (_meta, content) = grid.decode().map_err(|e| format!("{}: {e}", tr("say.qr_bad")))?;
    Ok(content)
}

/// The command the GUI gives the client's own binary.
///
/// The GUI installs nothing itself: it runs the very binary the customer
/// client ships, with the very command the headless run uses, and the
/// artefact that comes back is the same one the cabinet's panel issues.
///
/// The password travels in the pipe, never in the command line: the process
/// list on the machine is readable by everything, the pipe is not. The
/// command is kept apart from the spawn so that it can be read — and
/// checked — without a box.
pub fn oss_command(
    box_addr: &str,
    binary: &str,
    mwg_port: u16,
    kvik_port: u16,
    state: &str,
    key: &str,
    artefact: &str,
    ssh_key: &str,
) -> Vec<String> {
    let mut args = vec![
        "--box".to_owned(),
        box_addr.to_owned(),
        "--password-stdin".to_owned(),
        "--binary".to_owned(),
        binary.to_owned(),
        "--mwg-port".to_owned(),
        mwg_port.to_string(),
        "--mkvik-port".to_owned(),
        kvik_port.to_string(),
        "--state".to_owned(),
        state.to_owned(),
        "--key-out".to_owned(),
        key.to_owned(),
        "--out".to_owned(),
        artefact.to_owned(),
    ];
    // The operator's own key for this box, when the person named one. Without
    // it the client mints a key of its own and plants it with the password.
    if !ssh_key.trim().is_empty() {
        args.push("--ssh-key".to_owned());
        args.push(ssh_key.trim().to_owned());
    }
    args
}

/// A port from the field, when the field holds one.
fn port_of(raw: &str) -> Option<u16> {
    raw.trim().parse::<u16>().ok().filter(|port| *port > 0)
}

pub(crate) fn split_host(raw: &str) -> Option<(String, u16)> {
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
    Config,
    Export,
    Copy,
    Scan,
    File,
    Clip,
    InviteConn,
    InviteFull,
    MyKey,
    Unlink,
    Forget,
    PageNew,
    PageKnown,
}

enum Choice {
    Kill,
    Split,
    Lang,
    Style,
    Build,
    Key,
    Carry,
}

enum Row {
    Field { rect: Rect, label: &'static str, focus: Focus },
    Button { rect: Rect, text: ButtonText },
    Choice { rect: Rect, label: &'static str, which: Choice },
    Kit { rect: Rect, kit: Kit },
    Server { rect: Rect, index: usize },
    Guest { rect: Rect, index: usize, revoke: Rect },
    Entry { rect: Rect, index: usize, site: bool },
    Line { x: f64, y: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HitKind {
    Install,
    Remove,
    Fetch,
    Forget,
    Server(usize),
    /// `true` opens the new-box form. `false` opens the boxes already known.
    ServerPage(bool),
    Export,
    Copy,
    Scan,
    File,
    Clip,
    Kill,
    Split,
    Lang,
    Style,
    Build,
    Key,
    MyKey,
    Invite(bool),
    Unlink,
    Carry,
    Kit(Kit),
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

    /// One button across the whole row — the row where a pair would leave a
    /// half-empty half.
    fn solo(&mut self, id: (&'static str, ButtonText, HitKind)) {
        let rect = self.row_rect();
        self.rows.push(Row::Button { rect, text: id.1 });
        self.hit(id.0.to_owned(), rect, id.2);
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

    fn kit_card(&mut self, kit: Kit) {
        let rect = self.row_rect();
        self.rows.push(Row::Kit { rect, kit });
        let id = match kit {
            Kit::QuicMirage => "st-kit-quic-mirage",
            Kit::Quic => "st-kit-quic",
            Kit::All => "st-kit-all",
        };
        self.hit(id.to_owned(), rect, HitKind::Kit(kit));
    }

    fn server(&mut self, index: usize) {
        let rect = self.row_rect();
        self.rows.push(Row::Server { rect, index });
        self.hit(format!("st-srv-{index}"), rect, HitKind::Server(index));
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
        Section::Servers => {
            lay.pair(
                ("st-page-new", ButtonText::PageNew, HitKind::ServerPage(true)),
                ("st-page-known", ButtonText::PageKnown, HitKind::ServerPage(false)),
            );
            if stand.server_new {
                lay.field("st-host", tr("f.host"), Focus::Host, tr("f.host"), 240, false);
                lay.field("st-user", tr("f.user"), Focus::User, tr("f.user"), 240, false);
                lay.field("st-pass", tr("f.pass"), Focus::Pass, tr("f.pass"), 240, true);
                lay.choice("st-build", tr("f.build"), Choice::Build, HitKind::Build);
                if stand.build == Build::Own {
                    lay.field("st-bin", tr("f.binary"), Focus::Binary, tr("f.binary"), 240, false);
                    lay.choice("st-keysrc", tr("f.keysrc"), Choice::Key, HitKind::Key);
                    if stand.key_source == KeySource::File {
                        lay.field("st-key", tr("f.sshkey"), Focus::SshKey, tr("f.sshkey"), 240, false);
                    }
                    lay.field("st-mwg", tr("f.mwg"), Focus::MwgPort, tr("f.port"), 5, false);
                    lay.field("st-kvik", tr("f.kvik"), Focus::KvikPort, tr("f.port"), 5, false);
                    lay.pair(
                        ("st-go", ButtonText::Install, HitKind::Install),
                        ("st-mykey", ButtonText::MyKey, HitKind::MyKey),
                    );
                } else {
                    for kit in [Kit::Quic, Kit::QuicMirage, Kit::All] {
                        lay.kit_card(kit);
                    }
                    lay.solo(("st-go", ButtonText::Install, HitKind::Install));
                }
            } else {
                let servers = known_servers(stand);
                for index in 0..servers.len() {
                    lay.server(index);
                }
                let open = servers.iter().any(|host| host == &stand.open_host);
                if open {
                    for kit in [Kit::Quic, Kit::QuicMirage, Kit::All] {
                        lay.kit_card(kit);
                    }
                    lay.field("st-user", tr("f.user"), Focus::User, tr("f.user"), 240, false);
                    lay.field("st-pass", tr("f.pass"), Focus::Pass, tr("f.pass"), 240, true);
                    lay.pair(
                        ("st-go", ButtonText::Install, HitKind::Install),
                        ("st-config", ButtonText::Config, HitKind::Fetch),
                    );
                    lay.pair(
                        ("st-down", ButtonText::Remove, HitKind::Remove),
                        ("st-forget", ButtonText::Forget, HitKind::Forget),
                    );
                }
            }
        }
        Section::Device => {
            lay.choice("st-lang", tr("f.lang"), Choice::Lang, HitKind::Lang);
            lay.choice("st-style", tr("f.style"), Choice::Style, HitKind::Style);
            lay.choice("st-carry", tr("f.carry"), Choice::Carry, HitKind::Carry);
            lay.choice("st-kill", tr("f.kill"), Choice::Kill, HitKind::Kill);
            lay.field("st-dns", tr("f.dns"), Focus::Dns, tr("f.resolver"), 240, false);
            if stand.kill != Kill::Off {
                lay.field("st-except", tr("f.except"), Focus::Except, tr("f.dns_list"), 240, false);
            }
            lay.choice("st-split", tr("f.routes"), Choice::Split, HitKind::Split);
            lay.field("st-site", tr("f.site"), Focus::Site, tr("f.domain"), 240, false);
            for index in 0..stand.sites.len() {
                lay.entry(index, true);
            }
            lay.field("st-app", tr("f.app"), Focus::App, tr("f.process"), 240, false);
            for index in 0..stand.apps.len() {
                lay.entry(index, false);
            }
            lay.field("st-link", tr("f.link"), Focus::Link, tr("f.paste"), 256 * 1024, false);
            lay.pair(
                ("st-export", ButtonText::Export, HitKind::Export),
                ("st-copy", ButtonText::Copy, HitKind::Copy),
            );
            lay.thirds(
                ("st-scan", ButtonText::Scan, HitKind::Scan),
                ("st-file", ButtonText::File, HitKind::File),
                ("st-clip", ButtonText::Clip, HitKind::Clip),
            );
            lay.field("st-guest", tr("f.guest"), Focus::Guest, tr("f.name"), 64, false);
            lay.pair(
                ("st-invite-conn", ButtonText::InviteConn, HitKind::Invite(false)),
                ("st-invite-full", ButtonText::InviteFull, HitKind::Invite(true)),
            );
            for index in 0..stand.guests.len() {
                lay.guest(index);
            }
        }
        Section::Cabinet => {
            if stand.cabinet_bound {
                lay.solo(("st-unlink", ButtonText::Unlink, HitKind::Unlink));
            } else {
                lay.field("st-bind", tr("f.cabinet"), Focus::Bind, tr("bind.ph"), 512, false);
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

fn nav_buttons(content: Rect) -> [NavBtn; 3] {
    const IDS: [&str; 3] = ["st-nav-servers", "st-nav-device", "st-nav-cabinet"];
    let labels = [tr("nav.servers"), tr("nav.device"), tr("nav.cabinet")];
    const SECS: [Section; 3] = [Section::Servers, Section::Device, Section::Cabinet];
    let y = content.y + 6.0;
    let x0 = content.x + 16.0;
    let inner = (content.width - 32.0).max(0.0);
    let w = inner / 3.0;
    let mut out = [NavBtn {
        id: IDS[0],
        label: labels[0],
        section: Section::Servers,
        rect: Rect::new(0.0, 0.0, 0.0, 0.0),
    }; 3];
    for i in 0..3 {
        out[i] = NavBtn {
            id: IDS[i],
            label: labels[i],
            section: SECS[i],
            rect: Rect::new(x0 + i as f64 * w, y, w.max(0.0), 30.0),
        };
    }
    out
}

fn qr_payload(stand: &Stand) -> Option<&str> {
    if stand.section != Section::Device {
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
        HitKind::Fetch => stand.fetch_config(),
        HitKind::Forget => stand.forget_server(),
        HitKind::ServerPage(fresh) => {
            stand.server_new = fresh;
            if fresh {
                stand.open_host.clear();
            }
        }
        HitKind::Server(index) => {
            let hosts = known_servers(stand);
            if let Some(host) = hosts.get(index) {
                let host = host.clone();
                stand.open_server(&host);
            }
        }
        HitKind::Export => stand.export(),
        HitKind::Copy => stand.copy_link(),
        HitKind::Scan => stand.request_scan(),
        HitKind::File => stand.import_file(),
        HitKind::Clip => stand.import_clipboard(),
        HitKind::Kill => stand.cycle_kill(),
        HitKind::Split => stand.toggle_split(),
        HitKind::Lang => crate::text::cycle(),
        HitKind::Style => crate::theme::cycle(),
        HitKind::Build => stand.cycle_build(),
        HitKind::Key => stand.cycle_key_source(),
        HitKind::MyKey => stand.show_my_key(),
        HitKind::Unlink => stand.unlink = true,
        HitKind::Carry => stand.cycle_carry(),
        HitKind::Kit(kit) => stand.select_kit(kit),
        // The ready sheet does not build these. The arms stay so a hit of
        // that kind is still a defined action if one is ever delivered.
        HitKind::Proto(_) | HitKind::ProtoOn(_) => {}
        HitKind::Invite(full) => stand.invite(full),
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
    crate::theme::paint_sheet(ctx, g.panel.x, g.panel.y, g.panel.width, g.panel.height);

    paint_nav(ctx, g.content, stand.section);

    let status = stand.status();
    let note_line = note.unwrap_or("");
    let show = !status.is_empty() || !note_line.is_empty();
    let line = if !status.is_empty() { status } else { note_line.to_owned() };
    let lay = layout(stand, g.content, show);

    ctx.set_font("13px PT Root UI");
    ctx.set_text_baseline(TextBaseline::Middle);
    for row in &lay.rows {
        paint_row(ctx, stand, row, &line);
    }
    if let Some(text) = qr_payload(stand) {
        paint_qr(ctx, g, text);
    }
}

fn paint_nav(ctx: &mut dyn RenderContext, content: Rect, section: Section) {
    let nav = nav_buttons(content);
    let track = Rect::new(
        nav[0].rect.x,
        nav[0].rect.y,
        nav[2].rect.x + nav[2].rect.width - nav[0].rect.x,
        nav[0].rect.height,
    );
    let active = nav.iter().find(|n| n.section == section).map(|n| n.rect).unwrap_or(nav[0].rect);
    crate::theme::paint_segment(ctx, track, active);
    ctx.set_font("13px PT Root UI");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_text_baseline(TextBaseline::Alphabetic);
    let light = crate::theme::light();
    let idle = if light { "#5c5c64" } else { "#b4b4bc" };
    let hot = if light { "#141416" } else { "#f4f4f5" };
    for btn in &nav {
        ctx.set_fill_color(if btn.section == section { hot } else { idle });
        let y = crate::theme::label_baseline(btn.rect.y, btn.rect.height, 13.0);
        ctx.fill_text(btn.label, btn.rect.x + btn.rect.width / 2.0, y);
    }
}

fn paint_row(ctx: &mut dyn RenderContext, stand: &Stand, row: &Row, line: &str) {
    ctx.set_font("14px PT Root UI");
    ctx.set_text_baseline(TextBaseline::Alphabetic);
    match row {
        Row::Field { rect, label, focus } => {
            plate(ctx, *rect, stand.focus == *focus);
            ctx.set_text_align(TextAlign::Left);
            ctx.set_fill_color(dim());
            ctx.fill_text(label, rect.x + 8.0, crate::theme::label_baseline(rect.y, rect.height, 14.0));
        }
        Row::Button { rect, text } => {
            let primary = matches!(text, ButtonText::Install);
            let page = match text {
                ButtonText::PageNew => Some(stand.server_new),
                ButtonText::PageKnown => Some(!stand.server_new),
                _ => None,
            };
            if primary {
                ctx.set_fill_color(if crate::theme::light() { "#1c1c1f" } else { AMBER });
                ctx.fill_rounded_rect(rect.x, rect.y, rect.width, rect.height, 12.0);
                ctx.set_fill_color(if crate::theme::light() { "#ffffff" } else { "#1a1208" });
            } else {
                plate(ctx, *rect, page.unwrap_or(false));
                ctx.set_fill_color(if page == Some(true) { accent() } else { fg() });
            }
            ctx.set_text_align(TextAlign::Center);
            let label = button_label(stand, text);
            ctx.fill_text(label, rect.x + rect.width / 2.0, crate::theme::label_baseline(rect.y, rect.height, 14.0));
        }
        Row::Choice { rect, label, which } => {
            plate(ctx, *rect, false);
            ctx.set_text_align(TextAlign::Left);
            ctx.set_fill_color(dim());
            let y = crate::theme::label_baseline(rect.y, rect.height, 14.0);
            ctx.fill_text(label, rect.x + 8.0, y);
            ctx.set_fill_color(fg());
            ctx.set_text_align(TextAlign::Center);
            let value = choice_value(stand, which);
            let space = (rect.width - LABEL_W).max(0.0);
            ctx.fill_text(&value, rect.x + LABEL_W + space / 2.0, y);
        }
        Row::Kit { rect, kit } => {
            let selected = stand.kit == *kit;
            plate(ctx, *rect, selected);
            ctx.set_text_align(TextAlign::Left);
            ctx.set_font("14px PT Root UI");
            ctx.set_fill_color(if selected { accent() } else { fg() });
            ctx.fill_text(
                kit_title(*kit),
                rect.x + 8.0,
                crate::theme::label_baseline(rect.y, rect.height, 14.0),
            );
        }
        Row::Server { rect, index } => {
            let hosts = known_servers(stand);
            let Some(host) = hosts.get(*index) else {
                return;
            };
            let open = stand.open_host == *host;
            plate(ctx, *rect, open);
            ctx.set_text_align(TextAlign::Left);
            ctx.set_font("14px PT Root UI");
            ctx.set_fill_color(if open { accent() } else { fg() });
            ctx.fill_text(host, rect.x + 8.0, crate::theme::label_baseline(rect.y, rect.height, 14.0));
        }
        Row::Guest { rect, index, revoke } => {
            let Some(guest) = stand.guests.get(*index) else {
                return;
            };
            plate(ctx, *rect, stand.qr_guest == Some(*index));
            ctx.set_text_align(TextAlign::Left);
            ctx.set_fill_color(fg());
            let y = crate::theme::label_baseline(rect.y, rect.height, 14.0);
            ctx.fill_text(&short(&guest.name, 16), rect.x + 8.0, y);
            ctx.set_fill_color(dim());
            let kind = if guest.full { tr("b.invite_full") } else { tr("b.invite_conn") };
            ctx.fill_text(kind, rect.x + LABEL_W, y);
            ctx.set_text_align(TextAlign::Center);
            ctx.set_fill_color(accent());
            ctx.fill_text(tr("b.revoke"), revoke.x + revoke.width / 2.0, crate::theme::label_baseline(revoke.y, revoke.height, 14.0));
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
            ctx.set_fill_color(fg());
            let y = crate::theme::label_baseline(rect.y, rect.height, 14.0);
            ctx.fill_text(&short(text, 24), rect.x + 8.0, y);
            ctx.set_text_align(TextAlign::Right);
            ctx.set_fill_color(dim());
            ctx.fill_text(tr("b.drop"), rect.x + rect.width - 8.0, y);
        }
        Row::Line { x, y } => {
            ctx.set_text_align(TextAlign::Left);
            ctx.set_fill_color(dim());
            ctx.fill_text(line, *x, *y + 10.0);
        }
    }
}

fn plate(ctx: &mut dyn RenderContext, rect: Rect, hot: bool) {
    let (fill, line) = if crate::theme::light() {
        (
            if hot { "rgba(255,255,255,0.82)" } else { "rgba(255,255,255,0.5)" },
            "rgba(255,255,255,0.72)",
        )
    } else if hot {
        ("rgba(255,255,255,0.22)", "rgba(255,255,255,0.50)")
    } else {
        ("rgba(24,24,28,0.82)", "rgba(255,255,255,0.38)")
    };
    ctx.set_fill_color(fill);
    ctx.fill_rounded_rect(rect.x, rect.y, rect.width, rect.height, 10.0);
    ctx.set_stroke_color(line);
    ctx.set_stroke_width(1.0);
    ctx.set_line_dash(&[]);
    ctx.stroke_rounded_rect(rect.x, rect.y, rect.width, rect.height, 10.0);
}

fn button_label(stand: &Stand, text: &ButtonText) -> &'static str {
    let running = stand.bus.running.load(Ordering::Relaxed);
    match text {
        ButtonText::Install => {
            if running {
                tr("b.installing")
            } else if stand.open_host.is_empty() {
                tr("b.install")
            } else {
                tr("b.roll")
            }
        }
        ButtonText::Forget => tr("b.forget"),
        ButtonText::PageNew => tr("b.page_new"),
        ButtonText::PageKnown => tr("b.page_known"),
        ButtonText::Remove => {
            if running {
                tr("b.removing")
            } else {
                tr("b.remove")
            }
        }
        ButtonText::Config => {
            if running {
                tr("b.fetching")
            } else {
                tr("b.config")
            }
        }
        ButtonText::Export => tr("b.export"),
        ButtonText::Copy => tr("b.copy"),
        ButtonText::Scan => tr("b.scan"),
        ButtonText::File => tr("b.file"),
        ButtonText::Clip => tr("b.clip"),
        ButtonText::InviteConn => tr("b.invite_conn"),
        ButtonText::InviteFull => tr("b.invite_full"),
        ButtonText::MyKey => tr("b.my_key"),
        ButtonText::Unlink => tr("b.unlink"),
    }
}

fn choice_value(stand: &Stand, which: &Choice) -> String {
    match which {
        Choice::Kill => match stand.kill {
            Kill::Off => tr("b.off").into(),
            Kill::Soft => tr("c.soft").into(),
            Kill::Strict => tr("c.strict").into(),
        },
        Choice::Split => {
            if stand.split_bypass {
                tr("c.bypass").into()
            } else {
                tr("c.only").into()
            }
        }
        Choice::Carry => match stand.carry {
            Carry::Socks => tr("c.socks").into(),
            Carry::Tun => tr("c.tun").into(),
        },
        Choice::Lang => crate::text::current_name().into(),
        Choice::Style => {
            if crate::theme::light() {
                tr("theme.light").into()
            } else {
                tr("theme.dark").into()
            }
        }
        Choice::Build => match stand.build {
            Build::Ready => tr("c.build_ready").into(),
            Build::Own => tr("c.build_own").into(),
        },
        Choice::Key => match stand.key_source {
            KeySource::Own => tr("c.key_own").into(),
            KeySource::File => tr("c.key_file").into(),
        },
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
