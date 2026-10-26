//! Standalone sheet. Host, account, password, stack, install, the issued
//! link as a QR, export. The install itself is [`super::provision`].

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
const ROW: f64 = 40.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    None,
    Host,
    User,
    Pass,
    Stack,
    Link,
}

pub struct Stand {
    pub host: String,
    pub user: String,
    pub pass: String,
    pub stack_auto: bool,
    pub stack: String,
    pub focus: Focus,
    pub link: String,
    pub bus: Arc<InstallBus>,
    /// Bumped when an install takes the password, so each window clears that field once.
    pub pass_epoch: u64,
}

impl Stand {
    pub fn load() -> Self {
        let mut stand = Self {
            host: String::new(),
            user: "root".into(),
            pass: String::new(),
            stack_auto: true,
            stack: String::new(),
            focus: Focus::None,
            link: String::new(),
            bus: Arc::new(InstallBus::new()),
            pass_epoch: 0,
        };
        if let Some(dir) = dir() {
            if let Ok(text) = std::fs::read_to_string(dir.join("host.json")) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
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
                }
            }
            if let Ok(link) = std::fs::read_to_string(dir.join("link.txt")) {
                stand.link = link.trim().to_owned();
            }
        }
        stand
    }

    pub fn save_profile(&self) {
        let Some(dir) = dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let text = serde_json::json!({
            "host": self.host,
            "user": self.user,
            "stack_auto": self.stack_auto,
            "stack": self.stack,
        });
        if let Ok(body) = serde_json::to_string_pretty(&text) {
            let _ = std::fs::write(dir.join("host.json"), body);
        }
    }

    pub fn pull_bus(&mut self) {
        if let Some(link) = self.bus.link.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            if self.link != link {
                self.link = link;
            }
        }
    }

    pub fn status(&self) -> String {
        self.bus.text.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn install(&mut self) {
        if self.bus.running.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let host_port = split_host(&self.host);
        let Some((host, port)) = host_port else {
            *self.bus.text.lock().unwrap_or_else(|p| p.into_inner()) = "нужен хост".into();
            return;
        };
        if self.user.trim().is_empty() {
            *self.bus.text.lock().unwrap_or_else(|p| p.into_inner()) = "нужен логин".into();
            return;
        }
        self.save_profile();
        let stack = if self.stack_auto {
            "auto".to_owned()
        } else if self.stack.trim().is_empty() {
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
            },
            Arc::clone(&self.bus),
        );
    }

    pub fn export(&self) {
        if self.link.is_empty() {
            return;
        }
        let Some(path) = rfd::FileDialog::new().set_file_name("m1r4g3-link.txt").save_file() else {
            return;
        };
        let _ = std::fs::write(path, &self.link);
    }

    pub fn copy_link(&self) {
        if self.link.is_empty() {
            return;
        }
        if let Ok(mut clip) = arboard::Clipboard::new() {
            let _ = clip.set_text(self.link.clone());
        }
    }

    pub fn set_link(&mut self, link: &str) {
        let link = link.trim();
        if link.is_empty() || link.len() > 256 * 1024 {
            return;
        }
        self.link = link.to_owned();
        self.focus = Focus::Link;
        self.save_link();
    }

    pub fn save_link(&self) {
        let Some(dir) = dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("link.txt"), &self.link);
    }

    pub fn type_text(&mut self, text: &str) {
        let limit = if self.focus == Focus::Link { 256 * 1024 } else { 240 };
        let Some(buf) = self.buf_mut() else {
            return;
        };
        if buf.len() + text.len() > limit {
            return;
        }
        buf.push_str(text);
        if self.focus == Focus::Link {
            self.save_link();
        }
    }

    pub fn backspace(&mut self) {
        if let Some(buf) = self.buf_mut() {
            buf.pop();
        }
    }

    fn buf_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            Focus::Host => Some(&mut self.host),
            Focus::User => Some(&mut self.user),
            Focus::Pass => Some(&mut self.pass),
            Focus::Stack if !self.stack_auto => Some(&mut self.stack),
            Focus::Link => Some(&mut self.link),
            Focus::Stack | Focus::None => None,
        }
    }
}

fn dir() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("standalone"))
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

pub fn register<S: uzor_framework::Spec>(
    coord: &mut InputCoordinator,
    view: &VisualView<S>,
    layer: &LayerId,
    at: impl Fn(Rect) -> Rect,
    g: &Geom,
    stand: &mut Stand,
) {
    let rows = field_rects(g.content);
    for (id, rect, _) in &rows {
        let hit = match *id {
            "st-go" | "st-export" | "st-copy" => Some((*id, *rect)),
            "st-stack" if stand.stack_auto => Some((*id, *rect)),
            "st-stack" => Some((*id, Rect::new(rect.x, rect.y, 72.0, rect.height))),
            _ => None,
        };
        let Some((hit_id, hit_rect)) = hit else {
            continue;
        };
        coord.register_atomic(hit_id, WidgetKind::Button, at(hit_rect), Sense::CLICK, layer);
        if view.clicked(&WidgetId::from(hit_id)) {
            match hit_id {
                "st-go" => stand.install(),
                "st-export" => stand.export(),
                "st-copy" => stand.copy_link(),
                "st-stack" => {
                    stand.stack_auto = !stand.stack_auto;
                    stand.focus = Focus::None;
                    stand.save_profile();
                }
                _ => {}
            }
        }
    }
}

/// Height of the content rect. Fields, then the QR when a link is present.
pub fn content_height(has_link: bool) -> f64 {
    let fields = 8.0 + 6.0 * ROW + 32.0 + 8.0;
    if has_link { fields + 12.0 + 168.0 } else { fields }
}

pub fn field_rects(content: Rect) -> Vec<(&'static str, Rect, Focus)> {
    let mut y = content.y + 8.0;
    let x = content.x + 16.0;
    let w = (content.width - 32.0).max(0.0);
    let mut row = |id, focus| {
        let rect = Rect::new(x, y, w, 32.0);
        y += ROW;
        (id, rect, focus)
    };
    let mut out = vec![
        row("st-host", Focus::Host),
        row("st-user", Focus::User),
        row("st-pass", Focus::Pass),
        row("st-stack", Focus::Stack),
        row("st-link", Focus::Link),
        row("st-go", Focus::None),
    ];
    let half = (w - 8.0) / 2.0;
    out.push(("st-export", Rect::new(x, y, half.max(0.0), 32.0), Focus::None));
    out.push(("st-copy", Rect::new(x + half + 8.0, y, half.max(0.0), 32.0), Focus::None));
    out
}

pub fn paint(ctx: &mut dyn RenderContext, g: &Geom, stand: &Stand, _hot_sep: bool, note: Option<&str>) {
    ctx.set_fill_color("rgba(14,14,17,0.98)");
    ctx.fill_rect(g.panel.x, g.panel.y, g.panel.width, g.panel.height);
    ctx.set_fill_color("rgba(255,255,255,0.08)");
    ctx.fill_rect(0.0, g.tabbar.y + g.tabbar.height - 1.0, g.panel.width, 1.0);
    ctx.fill_rect(0.0, g.panel.y + g.panel.height - 1.0, g.panel.width, 1.0);

    let rows = field_rects(g.content);
    let status = stand.status();
    let line = if !status.is_empty() { status } else { note.unwrap_or("").to_owned() };
    ctx.set_font("13px sans-serif");
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.set_text_align(TextAlign::Left);
    ctx.set_fill_color(DIM);
    if !line.is_empty() {
        ctx.fill_text(&line, g.content.x + 16.0, g.content.y + 4.0);
    }
    for (id, rect, _) in &rows {
        let (label, value, secret) = match *id {
            "st-host" => ("хост", stand.host.as_str(), false),
            "st-user" => ("логин", stand.user.as_str(), false),
            "st-pass" => ("пароль", stand.pass.as_str(), true),
            "st-stack" => (
                "стек",
                if stand.stack_auto { "авто" } else { "свой" },
                false,
            ),
            "st-link" => ("ссылка", "", false),
            "st-go" => ("", if stand.bus.running.load(std::sync::atomic::Ordering::Relaxed) { "ставится…" } else { "установить" }, false),
            "st-export" => ("", "экспорт", false),
            "st-copy" => ("", "копировать", false),
            _ => ("", "", false),
        };
        let focused = match *id {
            "st-host" => stand.focus == Focus::Host,
            "st-user" => stand.focus == Focus::User,
            "st-pass" => stand.focus == Focus::Pass,
            "st-stack" => stand.focus == Focus::Stack && !stand.stack_auto,
            "st-link" => stand.focus == Focus::Link,
            _ => false,
        };
        ctx.set_fill_color(if focused { "rgba(251,178,106,0.10)" } else { "rgba(255,255,255,0.05)" });
        ctx.fill_rect(rect.x, rect.y, rect.width, rect.height);
        ctx.set_fill_color(if *id == "st-go" || *id == "st-export" || *id == "st-copy" { AMBER } else { FG });
        ctx.set_text_align(TextAlign::Left);
        if label.is_empty() {
            ctx.set_text_align(TextAlign::Center);
            ctx.fill_text(value, rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
        } else {
            ctx.set_fill_color(DIM);
            ctx.fill_text(label, rect.x + 8.0, rect.y + rect.height / 2.0);
            if *id == "st-stack" && stand.stack_auto {
                ctx.set_fill_color(FG);
                ctx.fill_text(value, rect.x + 72.0, rect.y + rect.height / 2.0);
            }
            let _ = secret;
        }
    }
    if !stand.link.is_empty() {
        paint_qr(ctx, g, &stand.link);
    }
}

fn link_preview(link: &str) -> String {
    if link.is_empty() {
        return "вставить".into();
    }
    if link.starts_with('{') {
        return format!("артефакт · {} б", link.len());
    }
    let count = link.chars().count();
    if count > 28 {
        let head: String = link.chars().take(28).collect();
        format!("{head}…")
    } else {
        link.to_owned()
    }
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
