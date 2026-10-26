//! The menu — a resizable, scrollable, TABBED container (the client's control
//! plane). Covers ~half the window by default; drag the bottom separator to
//! resize; content scrolls (wheel) inside a clipped viewport.
//!
//! Container mechanics (panel / tabbar / content-clip / separator / scrollbar)
//! are independent of content. Tabs: Connection · Setup. Skins live in Setup.

use uzor::core::types::Rect;
use uzor::render::{RenderContext, TextAlign, TextBaseline};

use crate::centerpiece::SkinKind;

pub const MENU_TOP: f64 = 32.0;
pub const TABBAR_H: f64 = 32.0;
pub const SEP_H: f64 = 8.0;
/// Default panel height as a fraction of the available area.
pub const DEFAULT_FRAC: f64 = 0.5;
pub const MIN_FRAC: f64 = 0.22;
pub const MAX_FRAC: f64 = 0.96;

// ── Palette ──────────────────────────────────────────────────────────────────────
const BG: &str = "rgba(14,14,17,0.98)";
const AMBER: &str = "#FBB26A";
const FG: &str = "#F4F4F5";
const MUTED: &str = "#878B91";
const DIMC: &str = "#6B6F7A";
const LINE: &str = "rgba(255,255,255,0.08)";

// ── Tabs ───────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuTab {
    Connection,
    Setup,
}

impl MenuTab {
    /// Every tab, in tab-bar order.
    pub const ALL: [MenuTab; 2] = [MenuTab::Connection, MenuTab::Setup];

    pub fn label(self) -> &'static str {
        match self {
            MenuTab::Connection => "Connect",
            MenuTab::Setup => "Setup",
        }
    }
}

// ── Content rows ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub enum MenuRow {
    ToggleSocks,
    ToggleHttp,
    ToggleTun,
    /// Sites this connection carries. Empty means every destination.
    Sites,
    /// Local programs this connection carries. Empty means every program.
    Apps,
    /// Exit mode segmented control: Auto | Manual.
    ExitMode,
    /// Cabinet countries, 3 per row, at most 3 rows visible. Scrolls inside
    /// itself when there are more rows.
    Countries,
    Skin(SkinKind),
    Action(MenuAction),
    /// First Setup row. A claim code or `/c/` link. Enter or BIND redeems it.
    Bind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    ImportFile,
    ImportText,
    ImportQrClip,
}

pub struct LaidRow {
    pub row: MenuRow,
    /// Content-local Y (before scroll).
    pub y: f64,
    pub h: f64,
    pub clickable: bool,
}

fn metrics(row: &MenuRow, country_count: usize, services: usize, apps: usize) -> (f64, bool) {
    match row {
        MenuRow::ToggleSocks | MenuRow::ToggleHttp | MenuRow::ToggleTun | MenuRow::Action(_) => {
            (40.0, true)
        }
        MenuRow::Sites => (list_block_height(services), true),
        MenuRow::Apps => (list_block_height(apps), true),
        MenuRow::ExitMode => (46.0, true),
        MenuRow::Countries => (country_block_height(country_count), true),
        MenuRow::Skin(_) => (38.0, true),
        MenuRow::Bind => (48.0, true),
    }
}

/// Cabinet sheet: the claim field only. Exit, transport, sites, apps, skins
/// and import are not rows. The desktop shell applies exit and transport itself.
#[cfg(not(target_os = "ios"))]
pub fn cabinet_rows() -> Vec<MenuRow> {
    vec![MenuRow::Bind]
}

/// Rows for the active tab. Manual mode on Connect opens one country grid.
pub fn tab_rows(tab: MenuTab, exit_auto: bool, country_count: usize) -> Vec<MenuRow> {
    match tab {
        MenuTab::Connection => {
            let mut rows = vec![MenuRow::ExitMode];
            if !exit_auto && country_count > 0 {
                rows.push(MenuRow::Countries);
            }
            rows.extend([
                MenuRow::ToggleSocks,
                MenuRow::ToggleHttp,
                MenuRow::ToggleTun,
                MenuRow::Sites,
                MenuRow::Apps,
            ]);
            rows
        }
        MenuTab::Setup => vec![
            MenuRow::Bind,
            MenuRow::Skin(SkinKind::Bloom),
            MenuRow::Skin(SkinKind::Planet),
            MenuRow::Action(MenuAction::ImportFile),
            MenuRow::Action(MenuAction::ImportText),
            MenuRow::Action(MenuAction::ImportQrClip),
        ],
    }
}

/// Lay out content rows (content-local Y from 0); returns rows + total height.
pub fn layout_rows(
    rows: &[MenuRow],
    country_count: usize,
    services: usize,
    apps: usize,
) -> (Vec<LaidRow>, f64) {
    let mut y = 6.0;
    let mut out = Vec::with_capacity(rows.len());
    for &row in rows {
        let (h, clickable) = metrics(&row, country_count, services, apps);
        out.push(LaidRow { row, y, h, clickable });
        y += h;
    }
    (out, y + 6.0)
}

// ── Country grid ─────────────────────────────────────────────────────────────

const COUNTRY_COLS: usize = 3;
const COUNTRY_VIS_ROWS: usize = 3;
const COUNTRY_INSET: f64 = 12.0;
const COUNTRY_GAP: f64 = 6.0;
const COUNTRY_CELL_H: f64 = 28.0;
const COUNTRY_PAD_Y: f64 = 6.0;
const COUNTRY_SCROLL_W: f64 = 10.0;

/// `(x, width)` of the Auto|Manual pill. The label holder is everything to its left.
pub fn exit_pill_box(w: f64) -> (f64, f64) {
    let pill_w = 140.0_f64.min((w - 108.0).max(72.0));
    let px = (w - 12.0 - pill_w).max(8.0);
    (px, pill_w)
}

fn country_row_count(n: usize) -> usize {
    if n == 0 {
        0
    } else {
        (n + COUNTRY_COLS - 1) / COUNTRY_COLS
    }
}

fn country_block_height(n: usize) -> f64 {
    let rows = country_row_count(n);
    if rows == 0 {
        return 0.0;
    }
    let vis = rows.min(COUNTRY_VIS_ROWS);
    let gaps = vis.saturating_sub(1) as f64;
    COUNTRY_PAD_Y * 2.0 + vis as f64 * COUNTRY_CELL_H + gaps * COUNTRY_GAP
}

/// Screen geometry of the country dropdown. `row_y` is the row's top in
/// screen space. `None` when there is nothing to list.
pub struct CountryScroll {
    pub viewport: Rect,
    pub max_scroll: f64,
    pub track: Option<Rect>,
    pub thumb: Option<Rect>,
    cell_w: f64,
}

pub fn country_scroll_geom(w: f64, row_y: f64, n: usize, scroll: f64) -> Option<CountryScroll> {
    let rows = country_row_count(n);
    if rows == 0 {
        return None;
    }
    let vis = rows.min(COUNTRY_VIS_ROWS);
    let scrolling = rows > COUNTRY_VIS_ROWS;
    let scroll_w = if scrolling { COUNTRY_SCROLL_W } else { 0.0 };
    let inner_w = (w - COUNTRY_INSET * 2.0 - scroll_w).max(0.0);
    let cell_w = ((inner_w - COUNTRY_GAP * (COUNTRY_COLS as f64 - 1.0)) / COUNTRY_COLS as f64).max(0.0);
    let vis_gaps = vis.saturating_sub(1) as f64;
    let inner_h = vis as f64 * COUNTRY_CELL_H + vis_gaps * COUNTRY_GAP;
    let content_h = rows as f64 * COUNTRY_CELL_H + (rows.saturating_sub(1) as f64) * COUNTRY_GAP;
    let max_scroll = (content_h - inner_h).max(0.0);
    let scroll = scroll.clamp(0.0, max_scroll);
    let viewport = Rect::new(COUNTRY_INSET, row_y + COUNTRY_PAD_Y, inner_w, inner_h);
    let (track, thumb) = if scrolling {
        let track = Rect::new(w - COUNTRY_INSET - 3.0, viewport.y, 3.0, inner_h);
        let thumb_h = (inner_h * (inner_h / content_h)).clamp(12.0, inner_h);
        let t = if max_scroll <= 0.0 { 0.0 } else { scroll / max_scroll };
        let thumb = Rect::new(track.x, viewport.y + (inner_h - thumb_h) * t, 3.0, thumb_h);
        (Some(track), Some(thumb))
    } else {
        (None, None)
    };
    Some(CountryScroll { viewport, max_scroll, track, thumb, cell_w })
}

fn country_cell(grid: &CountryScroll, index: usize, scroll: f64) -> Rect {
    let col = (index % COUNTRY_COLS) as f64;
    let row = (index / COUNTRY_COLS) as f64;
    let scroll = scroll.clamp(0.0, grid.max_scroll);
    Rect::new(
        grid.viewport.x + col * (grid.cell_w + COUNTRY_GAP),
        grid.viewport.y - scroll + row * (COUNTRY_CELL_H + COUNTRY_GAP),
        grid.cell_w,
        COUNTRY_CELL_H,
    )
}

/// Country under a screen point, or `None` on a gap or the scrollbar.
pub fn country_index_at(w: f64, row_y: f64, n: usize, scroll: f64, px: f64, py: f64) -> Option<usize> {
    let grid = country_scroll_geom(w, row_y, n, scroll)?;
    let v = grid.viewport;
    if px < v.x || py < v.y || px >= v.x + v.width || py >= v.y + v.height {
        return None;
    }
    let scroll = scroll.clamp(0.0, grid.max_scroll);
    let lx = px - v.x;
    let ly = py - v.y + scroll;
    let y_stride = COUNTRY_CELL_H + COUNTRY_GAP;
    let row = (ly / y_stride) as usize;
    if ly - row as f64 * y_stride > COUNTRY_CELL_H {
        return None;
    }
    let x_stride = grid.cell_w + COUNTRY_GAP;
    if x_stride <= 0.0 {
        return None;
    }
    let col = (lx / x_stride) as usize;
    if col >= COUNTRY_COLS || lx - col as f64 * x_stride > grid.cell_w {
        return None;
    }
    let index = row * COUNTRY_COLS + col;
    if index < n { Some(index) } else { None }
}

// ── Site / app list block ────────────────────────────────────────────────────

const LIST_HEADER: f64 = 28.0;
const LIST_FIELD: f64 = 36.0;
const LIST_CHIP_H: f64 = 24.0;
const LIST_CHIP_GAP: f64 = 4.0;
const LIST_COLS: usize = 2;
const LIST_PAD: f64 = 8.0;

fn list_chip_rows(n: usize) -> usize {
    if n == 0 { 0 } else { (n + LIST_COLS - 1) / LIST_COLS }
}

fn list_block_height(n: usize) -> f64 {
    let rows = list_chip_rows(n);
    let chips = if rows == 0 {
        0.0
    } else {
        6.0 + rows as f64 * LIST_CHIP_H + (rows - 1) as f64 * LIST_CHIP_GAP
    };
    LIST_HEADER + LIST_FIELD + chips + LIST_PAD
}

pub enum ListPart {
    /// Header glyph. Clears the names on this side.
    Clear,
    Field,
    File,
    /// The x on a chip.
    Chip(usize),
    /// TUN only. The role word on a chip.
    Role(usize),
}

/// Hit inside a SITES or APPS block. `row_y` is the block top in screen space.
/// `roles` is set only while TUN is the selected mode: the word left of x
/// flips that name between tunnel and bypass.
pub fn list_part_at(w: f64, row_y: f64, n: usize, px: f64, py: f64, roles: bool) -> Option<ListPart> {
    let h = list_block_height(n);
    if px < 0.0 || py < row_y || px > w || py >= row_y + h {
        return None;
    }
    let local = py - row_y;
    if local < LIST_HEADER {
        if px >= w - 84.0 { Some(ListPart::Clear) } else { Some(ListPart::Field) }
    } else if local < LIST_HEADER + LIST_FIELD {
        if px >= w - 68.0 { Some(ListPart::File) } else { Some(ListPart::Field) }
    } else {
        let origin = row_y + LIST_HEADER + LIST_FIELD + 6.0;
        let col_w = (((w - 24.0) - LIST_CHIP_GAP) / LIST_COLS as f64).max(0.0);
        let lx = px - 12.0;
        let ly = py - origin;
        if lx < 0.0 || ly < 0.0 || col_w <= 0.0 {
            return None;
        }
        let stride_y = LIST_CHIP_H + LIST_CHIP_GAP;
        let row = (ly / stride_y) as usize;
        if ly - row as f64 * stride_y > LIST_CHIP_H {
            return None;
        }
        let stride_x = col_w + LIST_CHIP_GAP;
        let col = (lx / stride_x) as usize;
        if col >= LIST_COLS || lx - col as f64 * stride_x > col_w {
            return None;
        }
        let index = row * LIST_COLS + col;
        if index >= n {
            return None;
        }
        let cell = list_chip_rect(w, row_y, index);
        let x0 = cell.x + cell.width - 18.0;
        if px >= x0 {
            Some(ListPart::Chip(index))
        } else if roles && px >= x0 - 64.0 {
            Some(ListPart::Role(index))
        } else {
            None
        }
    }
}

fn list_chip_rect(w: f64, row_y: f64, index: usize) -> Rect {
    let col = (index % LIST_COLS) as f64;
    let row = (index / LIST_COLS) as f64;
    let col_w = (((w - 24.0) - LIST_CHIP_GAP) / LIST_COLS as f64).max(0.0);
    let origin = row_y + LIST_HEADER + LIST_FIELD + 6.0;
    Rect::new(
        12.0 + col * (col_w + LIST_CHIP_GAP),
        origin + row * (LIST_CHIP_H + LIST_CHIP_GAP),
        col_w,
        LIST_CHIP_H,
    )
}

// ── Geometry ─────────────────────────────────────────────────────────────────────

pub struct Geom {
    pub panel: Rect,
    pub tabbar: Rect,
    pub content: Rect,
    pub sep: Rect,
    pub tabs: Vec<Rect>,
}

/// Compute container rects for a panel of height `panel_h` (px, below chrome)
/// with `ntabs` equal-width tab columns. `top` is the sheet's top edge.
/// `tab_h` is the tab row. `lip` is the bottom edge. iOS keeps [`TABBAR_H`]
/// and [`SEP_H`] via [`geom`].
pub fn geom_at(w: f64, h: f64, panel_h: f64, ntabs: usize, top: f64, tab_h: f64, lip: f64) -> Geom {
    let panel = Rect::new(0.0, top, w, panel_h);
    let tabbar = Rect::new(0.0, top, w, tab_h);
    let content = Rect::new(
        0.0,
        top + tab_h,
        w,
        (panel_h - tab_h - lip).max(0.0),
    );
    let sep = Rect::new(0.0, top + panel_h - lip, w, lip);
    let n = ntabs.max(1);
    let tw = w / n as f64;
    let tabs = (0..n)
        .map(|i| Rect::new(tw * i as f64, top, tw, tab_h))
        .collect();
    let _ = h;
    Geom { panel, tabbar, content, sep, tabs }
}

pub fn geom(w: f64, h: f64, panel_h: f64, ntabs: usize) -> Geom {
    geom_at(w, h, panel_h, ntabs, MENU_TOP, TABBAR_H, SEP_H)
}

// ── Draw ───────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn draw_menu(
    ctx: &mut dyn RenderContext,
    w: f64,
    h: f64,
    g: &Geom,
    tab: MenuTab,
    tabs: &[MenuTab],
    laid: &[LaidRow],
    content_h: f64,
    scroll: f64,
    country_scroll: f64,
    skin: SkinKind,
    countries: &[String],
    selected: Option<&str>,
    exit_auto: bool,
    note: Option<&str>,
    mode: &str,
    socks_on: bool,
    http_on: bool,
    tun_on: bool,
    services: &[String],
    apps: &[String],
    bypass_services: &[String],
    bypass_apps: &[String],
    sites_focused: bool,
    apps_focused: bool,
    list_draft: &str,
    bind_focused: bool,
    bind_draft: &str,
    bound: bool,
    unlink_armed: bool,
    cabinet: bool,
    bound_name: Option<&str>,
    marquee: f64,
    log: &[String],
    log_open: bool,
    file_hover: bool,
    pointer: Option<(f64, f64)>,
) {
    // Panel. The scene under the sheet stays as it is. No logo-color stripe.
    ctx.set_fill_color(BG);
    ctx.fill_rect(g.panel.x, g.panel.y, g.panel.width, g.panel.height);

    // ── Tab bar ──────────────────────────────────────────────────────────────
    for (i, tb) in tabs.iter().enumerate() {
        let r = g.tabs[i];
        let active = *tb == tab;
        if active {
            ctx.set_fill_color("rgba(251,178,106,0.12)");
            ctx.fill_rect(r.x, r.y, r.width, r.height);
            ctx.set_fill_color(AMBER);
            ctx.fill_rect(r.x, r.y + r.height - 2.0, r.width, 2.0);
        }
        ctx.set_fill_color(if active { AMBER } else { MUTED });
        ctx.set_font("12px sans-serif");
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        fill_clipped(ctx, tb.label(), r.x + r.width / 2.0, r.y + r.height / 2.0, r.x, r.y, r.width, r.height);
    }
    ctx.set_fill_color(LINE);
    ctx.fill_rect(0.0, g.tabbar.y + g.tabbar.height - 1.0, w, 1.0);

    // ── Content (clipped + scrolled) ───────────────────────────────────────────
    ctx.save();
    ctx.clip_rect(g.content.x, g.content.y, g.content.width, g.content.height);
    let cy = g.content.y - scroll;
    for lr in laid {
        let ry = cy + lr.y;
        if ry + lr.h < g.content.y || ry > g.content.y + g.content.height {
            continue; // culled
        }
        draw_row(
            ctx, w, lr, ry, country_scroll, skin, countries, selected, exit_auto, mode,
            socks_on, http_on, tun_on, services, apps, bypass_services, bypass_apps,
            sites_focused, apps_focused, list_draft, bind_focused, bind_draft, bound, unlink_armed, cabinet,
            bound_name,
            file_hover, pointer,
        );
    }
    ctx.restore();

    // ── Scrollbar ──────────────────────────────────────────────────────────────
    if content_h > g.content.height {
        let track_h = g.content.height;
        let thumb_h = (track_h * (track_h / content_h)).max(24.0);
        let max_scroll = (content_h - track_h).max(1.0);
        let ty = g.content.y + (track_h - thumb_h) * (scroll / max_scroll).clamp(0.0, 1.0);
        ctx.set_fill_color("rgba(255,255,255,0.14)");
        ctx.fill_rect(w - 4.0, ty, 3.0, thumb_h);
    }

    // Same hairline as the rule under the chrome. Not a drag handle.
    ctx.set_fill_color(LINE);
    ctx.fill_rect(0.0, g.panel.y + g.panel.height - 1.0, w, 1.0);

    if let Some(n) = note {
        draw_ticker(ctx, w, g, n, marquee, log, log_open);
    }
}

fn draw_ticker(
    ctx: &mut dyn RenderContext,
    w: f64,
    g: &Geom,
    text: &str,
    marquee: f64,
    log: &[String],
    log_open: bool,
) {
    let strip_y = g.panel.y + g.panel.height - 26.0;
    let strip_h = 22.0;
    ctx.set_fill_color("#0C0C10");
    ctx.fill_rect(0.0, strip_y, w, strip_h);
    let width = text.chars().count() as f64 * 7.2;
    let view = (w - 16.0).max(0.0);
    let x = if width <= view {
        (w - width) / 2.0
    } else {
        let span = width + 48.0;
        8.0 - (marquee * 36.0) % span
    };
    ctx.set_fill_color(AMBER);
    ctx.set_font("13px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    fill_clipped(ctx, text, x, strip_y + strip_h / 2.0, 8.0, strip_y, view, strip_h);
    if !log_open || log.is_empty() {
        return;
    }
    let shown = log.len().min(8);
    let start = log.len() - shown;
    let ph = shown as f64 * 16.0 + 8.0;
    let top = (strip_y - ph - 2.0).max(MENU_TOP);
    ctx.set_fill_color("#141418");
    ctx.fill_rect(8.0, top, (w - 16.0).max(0.0), ph);
    ctx.set_fill_color(AMBER);
    ctx.fill_rect(8.0, top, (w - 16.0).max(0.0), 1.0);
    ctx.set_font("12px sans-serif");
    ctx.set_fill_color(FG);
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    for (i, line) in log[start..].iter().enumerate() {
        let y = top + 12.0 + i as f64 * 16.0;
        fill_clipped(ctx, line, 16.0, y, 12.0, top + 2.0, (w - 28.0).max(0.0), 16.0);
    }
}

fn draw_list_block(
    ctx: &mut dyn RenderContext,
    w: f64,
    ry: f64,
    h: f64,
    label: &str,
    names: &[String],
    bypass: &[String],
    roles: bool,
    focused: bool,
    draft: &str,
    file_hover: bool,
    pointer: Option<(f64, f64)>,
) {
    let hovered = file_hover && pointer.map(|(px, py)| px >= 0.0 && py >= ry && px <= w && py < ry + h).unwrap_or(false);
    if hovered {
        ctx.set_fill_color("rgba(251,178,106,0.35)");
        ctx.fill_rect(8.0, ry + 1.0, (w - 16.0).max(0.0), 1.0);
        ctx.fill_rect(8.0, ry + h - 2.0, (w - 16.0).max(0.0), 1.0);
        ctx.fill_rect(8.0, ry + 1.0, 1.0, (h - 2.0).max(0.0));
        ctx.fill_rect(w - 9.0, ry + 1.0, 1.0, (h - 2.0).max(0.0));
    }
    let count = names.len() + if roles { bypass.len() } else { 0 };
    ctx.set_font("14px sans-serif");
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.set_fill_color(FG);
    ctx.set_text_align(TextAlign::Left);
    fill_clipped(ctx, label, 20.0, ry + LIST_HEADER / 2.0, 12.0, ry, 64.0, LIST_HEADER);
    if roles {
        ctx.set_fill_color(AMBER);
        ctx.set_font("11px sans-serif");
        fill_clipped(
            ctx,
            "TUNNEL / BYPASS",
            78.0,
            ry + LIST_HEADER / 2.0,
            74.0,
            ry,
            (w - 168.0).max(0.0),
            LIST_HEADER,
        );
    }
    if count > 0 {
        ctx.set_fill_color(AMBER);
        ctx.set_text_align(TextAlign::Right);
        fill_clipped(ctx, "CLEAR", w - 18.0, ry + LIST_HEADER / 2.0, w - 78.0, ry, 64.0, LIST_HEADER);
    }

    let file_x = w - 64.0;
    let field_x = 12.0;
    let field_w = (file_x - 8.0 - field_x).max(0.0);
    let field_y = ry + LIST_HEADER + 4.0;
    let field_h = LIST_FIELD - 8.0;
    ctx.set_fill_color(if focused { "rgba(251,178,106,0.10)" } else { "rgba(255,255,255,0.05)" });
    ctx.fill_rect(field_x, field_y, field_w, field_h);
    if focused {
        ctx.set_fill_color(AMBER);
        ctx.fill_rect(field_x, field_y + field_h - 2.0, field_w, 2.0);
    }
    ctx.set_font("14px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    let shown = if focused { draft } else { "" };
    if shown.is_empty() {
        ctx.set_fill_color(DIMC);
        fill_clipped(ctx, "type or drop", field_x + 8.0, field_y + field_h / 2.0, field_x + 4.0, field_y, (field_w - 8.0).max(0.0), field_h);
    } else {
        ctx.set_fill_color(FG);
        fill_clipped(ctx, shown, field_x + 8.0, field_y + field_h / 2.0, field_x + 4.0, field_y, (field_w - 12.0).max(0.0), field_h);
    }
    if focused {
        let caret_x = (field_x + 8.0 + shown.chars().count() as f64 * 7.2).min(field_x + field_w - 6.0);
        ctx.set_fill_color(AMBER);
        ctx.fill_rect(caret_x, field_y + 6.0, 1.0, (field_h - 12.0).max(0.0));
    }
    ctx.set_fill_color("rgba(251,178,106,0.16)");
    ctx.fill_rect(file_x, field_y, 48.0, field_h);
    ctx.set_fill_color(AMBER);
    ctx.set_font("12px sans-serif");
    ctx.set_text_align(TextAlign::Center);
    fill_clipped(ctx, "FILE", file_x + 24.0, field_y + field_h / 2.0, file_x, field_y, 48.0, field_h);

    let mut index = 0usize;
    let mut paint = |name: &str, bypassed: bool| {
        let cell = list_chip_rect(w, ry, index);
        index += 1;
        ctx.set_fill_color("rgba(255,255,255,0.06)");
        ctx.fill_rect(cell.x, cell.y, cell.width, cell.height);
        let name_w = if roles { (cell.width - 90.0).max(0.0) } else { (cell.width - 22.0).max(0.0) };
        ctx.set_fill_color(FG);
        ctx.set_font("12px sans-serif");
        ctx.set_text_align(TextAlign::Left);
        ctx.set_text_baseline(TextBaseline::Middle);
        fill_clipped(ctx, name, cell.x + 8.0, cell.y + cell.height / 2.0, cell.x + 4.0, cell.y, name_w, cell.height);
        if roles {
            ctx.set_fill_color(if bypassed { MUTED } else { AMBER });
            ctx.set_font("11px sans-serif");
            ctx.set_text_align(TextAlign::Right);
            let role = if bypassed { "BYPASS" } else { "TUNNEL" };
            fill_clipped(
                ctx,
                role,
                cell.x + cell.width - 22.0,
                cell.y + cell.height / 2.0,
                cell.x + cell.width - 84.0,
                cell.y,
                60.0,
                cell.height,
            );
        }
        ctx.set_fill_color(DIMC);
        ctx.set_font("12px sans-serif");
        ctx.set_text_align(TextAlign::Right);
        fill_clipped(ctx, "x", cell.x + cell.width - 6.0, cell.y + cell.height / 2.0, cell.x + cell.width - 16.0, cell.y, 14.0, cell.height);
    };
    for name in names {
        paint(name, false);
    }
    if roles {
        for name in bypass {
            paint(name, true);
        }
    }
}

/// Draw `text` and clip the glyphs to the holder. The holder is what stretches
/// with the window; the glyphs must not paint outside it.
fn fill_clipped(
    ctx: &mut dyn RenderContext,
    text: &str,
    x: f64,
    y: f64,
    clip_x: f64,
    clip_y: f64,
    clip_w: f64,
    clip_h: f64,
) {
    if clip_w <= 0.0 || clip_h <= 0.0 {
        return;
    }
    ctx.save();
    ctx.clip_rect(clip_x, clip_y, clip_w, clip_h);
    ctx.fill_text(text, x, y);
    ctx.restore();
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    ctx: &mut dyn RenderContext,
    w: f64,
    lr: &LaidRow,
    ry: f64,
    country_scroll: f64,
    skin: SkinKind,
    countries: &[String],
    selected: Option<&str>,
    exit_auto: bool,
    mode: &str,
    socks_on: bool,
    http_on: bool,
    tun_on: bool,
    services: &[String],
    apps: &[String],
    bypass_services: &[String],
    bypass_apps: &[String],
    sites_focused: bool,
    apps_focused: bool,
    list_draft: &str,
    bind_focused: bool,
    bind_draft: &str,
    bound: bool,
    unlink_armed: bool,
    cabinet: bool,
    bound_name: Option<&str>,
    file_hover: bool,
    pointer: Option<(f64, f64)>,
) {
    match lr.row {
        MenuRow::ToggleSocks | MenuRow::ToggleHttp | MenuRow::ToggleTun => {
            // The row picks where traffic goes. The globe is the up/down signal.
            let _ = (socks_on, http_on, tun_on);
            let (label, picked) = match lr.row {
                MenuRow::ToggleSocks => ("SOCKS5", mode == "socks"),
                MenuRow::ToggleHttp => ("HTTP", mode == "http"),
                MenuRow::ToggleTun => ("TUN", mode == "tun"),
                _ => return,
            };
            if picked {
                ctx.set_fill_color(AMBER);
                ctx.begin_path();
                ctx.arc(10.0, ry + lr.h / 2.0, 3.0, 0.0, std::f64::consts::TAU);
                ctx.fill();
            }
            ctx.set_font("14px sans-serif");
            ctx.set_text_baseline(TextBaseline::Middle);
            ctx.set_fill_color(if picked { AMBER } else { FG });
            ctx.set_text_align(TextAlign::Left);
            fill_clipped(ctx, label, 20.0, ry + lr.h / 2.0, 16.0, ry, (w - 24.0).max(0.0), lr.h);
        }
        MenuRow::Sites | MenuRow::Apps => {
            let roles = mode == "tun";
            let (label, names, bypass, focused) = match lr.row {
                MenuRow::Sites => ("SITES", services, bypass_services, sites_focused),
                MenuRow::Apps => ("APPS", apps, bypass_apps, apps_focused),
                _ => return,
            };
            draw_list_block(ctx, w, ry, lr.h, label, names, bypass, roles, focused, list_draft, file_hover, pointer);
        }
        MenuRow::ExitMode => {
            ctx.set_font("14px sans-serif");
            ctx.set_text_baseline(TextBaseline::Middle);
            // Pill stays inside the row. The label holder ends where the pill starts.
            let pill_h = 26.0;
            let (px, pill_w) = exit_pill_box(w);
            let py = ry + (lr.h - pill_h) / 2.0;
            ctx.set_fill_color(FG);
            ctx.set_text_align(TextAlign::Left);
            fill_clipped(ctx, "EXIT", 20.0, ry + lr.h / 2.0, 12.0, ry, (px - 16.0).max(0.0), lr.h);
            ctx.set_fill_color("rgba(255,255,255,0.05)");
            ctx.fill_rect(px, py, pill_w, pill_h);
            let half = pill_w / 2.0;
            let active_x = if exit_auto { px } else { px + half };
            ctx.set_fill_color("rgba(251,178,106,0.16)");
            ctx.fill_rect(active_x, py, half, pill_h);
            ctx.set_fill_color(AMBER);
            ctx.fill_rect(active_x, py + pill_h - 2.0, half, 2.0);
            ctx.set_font("12px sans-serif");
            ctx.set_text_align(TextAlign::Center);
            ctx.set_fill_color(if exit_auto { AMBER } else { MUTED });
            fill_clipped(ctx, "Auto", px + half / 2.0, py + pill_h / 2.0, px, py, half, pill_h);
            ctx.set_fill_color(if exit_auto { MUTED } else { AMBER });
            fill_clipped(ctx, "Manual", px + half + half / 2.0, py + pill_h / 2.0, px + half, py, half, pill_h);
        }
        MenuRow::Countries => {
            let Some(grid) = country_scroll_geom(w, ry, countries.len(), country_scroll) else { return };
            let scroll = country_scroll.clamp(0.0, grid.max_scroll);
            let v = grid.viewport;
            ctx.save();
            ctx.clip_rect(v.x, v.y, v.width, v.height);
            for (i, code) in countries.iter().enumerate() {
                let cell = country_cell(&grid, i, scroll);
                if cell.y + cell.height < v.y || cell.y > v.y + v.height {
                    continue;
                }
                let sel = selected == Some(code.as_str());
                ctx.set_fill_color(if sel { "rgba(251,178,106,0.18)" } else { "rgba(255,255,255,0.05)" });
                ctx.fill_rect(cell.x, cell.y, cell.width, cell.height);
                ctx.set_fill_color(if sel { AMBER } else { FG });
                ctx.set_font("12px sans-serif");
                ctx.set_text_align(TextAlign::Center);
                ctx.set_text_baseline(TextBaseline::Middle);
                fill_clipped(
                    ctx,
                    code,
                    cell.x + cell.width / 2.0,
                    cell.y + cell.height / 2.0,
                    cell.x + 2.0,
                    cell.y,
                    (cell.width - 4.0).max(0.0),
                    cell.height,
                );
            }
            ctx.restore();
            if let Some(thumb) = grid.thumb {
                ctx.set_fill_color("rgba(255,255,255,0.28)");
                ctx.fill_rect(thumb.x, thumb.y, thumb.width, thumb.height);
            }
        }
        MenuRow::Skin(s) => {
            let active = s == skin;
            if active {
                ctx.set_fill_color("rgba(251,178,106,0.10)");
                ctx.fill_rect(2.0, ry, w - 2.0, lr.h);
            }
            ctx.set_fill_color(if active { AMBER } else { FG });
            ctx.set_font("14px sans-serif");
            ctx.set_text_align(TextAlign::Left);
            ctx.set_text_baseline(TextBaseline::Middle);
            let name = match s {
                SkinKind::Bloom => "Bloom",
                SkinKind::Planet => "Planet",
            };
            fill_clipped(ctx, name, 20.0, ry + lr.h / 2.0, 12.0, ry, (w - 48.0).max(0.0), lr.h);
            if active {
                ctx.set_fill_color(AMBER);
                ctx.begin_path();
                ctx.arc(w - 22.0, ry + lr.h / 2.0, 3.5, 0.0, std::f64::consts::TAU);
                ctx.fill();
            }
        }
        MenuRow::Action(a) => {
            let label = match a {
                MenuAction::ImportFile => "Import  ·  from file",
                MenuAction::ImportText => "Import  ·  paste text",
                MenuAction::ImportQrClip => "Import  ·  paste QR",
            };
            ctx.set_fill_color(AMBER);
            ctx.set_font("14px sans-serif");
            ctx.set_text_align(TextAlign::Left);
            ctx.set_text_baseline(TextBaseline::Middle);
            fill_clipped(ctx, label, 20.0, ry + lr.h / 2.0, 12.0, ry, (w - 44.0).max(0.0), lr.h);
            ctx.set_fill_color(DIMC);
            ctx.set_text_align(TextAlign::Right);
            ctx.set_font("15px sans-serif");
            fill_clipped(ctx, "›", w - 18.0, ry + lr.h / 2.0, w - 36.0, ry, 24.0, lr.h);
        }
        MenuRow::Bind => {
            draw_bind_row(ctx, w, ry, lr.h, bind_focused, bind_draft, bound, unlink_armed, cabinet, bound_name)
        }
    }
}

/// Right edge of the BIND button. A click to the right of this submits.
pub fn bind_button_x(w: f64) -> f64 {
    (w - 76.0).max(120.0)
}

/// Left edge of ОТВЯЗАТЬ. A click on the name does nothing.
pub fn unlink_button_x(w: f64) -> f64 {
    (w - 134.0).max(96.0)
}

/// Left edge of КАБИНЕТ. The span ends at the unlink button.
pub fn cabinet_button_x(w: f64) -> f64 {
    (unlink_button_x(w) - 92.0).max(8.0)
}

fn draw_bind_row(
    ctx: &mut dyn RenderContext,
    w: f64,
    ry: f64,
    h: f64,
    focused: bool,
    draft: &str,
    bound: bool,
    unlink_armed: bool,
    cabinet: bool,
    bound_name: Option<&str>,
) {
    if bound {
        let button = if unlink_armed { "ПОДТВЕРДИТЬ" } else { "ОТВЯЗАТЬ" };
        let unlink_x = unlink_button_x(w);
        let bh = 32.0;
        let by = ry + (h - bh) / 2.0;
        let unlink_w = (w - 16.0 - unlink_x).max(0.0);
        let name_right = if cabinet { cabinet_button_x(w) } else { unlink_x };
        ctx.set_font("14px sans-serif");
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.set_text_align(TextAlign::Left);
        ctx.set_fill_color(FG);
        fill_clipped(
            ctx,
            bound_name.unwrap_or(""),
            16.0,
            ry + h / 2.0,
            12.0,
            ry,
            (name_right - 20.0).max(0.0),
            h,
        );
        if cabinet {
            let cabinet_x = cabinet_button_x(w);
            let cabinet_w = (unlink_x - 6.0 - cabinet_x).max(0.0);
            ctx.set_fill_color("rgba(255,255,255,0.08)");
            ctx.fill_rect(cabinet_x, by, cabinet_w, bh);
            ctx.set_fill_color(FG);
            ctx.set_text_align(TextAlign::Center);
            fill_clipped(
                ctx,
                "КАБИНЕТ",
                cabinet_x + cabinet_w / 2.0,
                by + bh / 2.0,
                cabinet_x,
                by,
                cabinet_w,
                bh,
            );
        }
        ctx.set_fill_color(if unlink_armed {
            "rgba(251,178,106,0.28)"
        } else {
            "rgba(251,178,106,0.16)"
        });
        ctx.fill_rect(unlink_x, by, unlink_w, bh);
        ctx.set_fill_color(AMBER);
        ctx.set_text_align(TextAlign::Center);
        fill_clipped(
            ctx,
            button,
            unlink_x + unlink_w / 2.0,
            by + bh / 2.0,
            unlink_x,
            by,
            unlink_w,
            bh,
        );
        return;
    }
    let button_x = bind_button_x(w);
    let field_x = 108.0;
    let field_w = (button_x - 8.0 - field_x).max(0.0);
    let field_h = 32.0;
    let field_y = ry + (h - field_h) / 2.0;
    ctx.set_font("14px sans-serif");
    ctx.set_text_baseline(TextBaseline::Middle);
    ctx.set_fill_color(FG);
    ctx.set_text_align(TextAlign::Left);
    fill_clipped(ctx, "CABINET", 16.0, ry + h / 2.0, 8.0, ry, 96.0, h);
    ctx.set_fill_color(if focused { "rgba(251,178,106,0.10)" } else { "rgba(255,255,255,0.05)" });
    ctx.fill_rect(field_x, field_y, field_w, field_h);
    if focused {
        ctx.set_fill_color(AMBER);
        ctx.fill_rect(field_x, field_y + field_h - 2.0, field_w, 2.0);
    }
    ctx.set_font("14px sans-serif");
    ctx.set_text_align(TextAlign::Left);
    ctx.set_text_baseline(TextBaseline::Middle);
    if draft.is_empty() {
        ctx.set_fill_color(DIMC);
        fill_clipped(
            ctx,
            "claim or /c/ link",
            field_x + 8.0,
            field_y + field_h / 2.0,
            field_x + 4.0,
            field_y,
            (field_w - 8.0).max(0.0),
            field_h,
        );
    } else {
        ctx.set_fill_color(FG);
        fill_clipped(
            ctx,
            draft,
            field_x + 8.0,
            field_y + field_h / 2.0,
            field_x + 4.0,
            field_y,
            (field_w - 12.0).max(0.0),
            field_h,
        );
    }
    if focused {
        let caret_x = (field_x + 8.0 + draft.chars().count() as f64 * 7.2).min(field_x + field_w - 6.0);
        ctx.set_fill_color(AMBER);
        ctx.fill_rect(caret_x, field_y + 6.0, 1.0, (field_h - 12.0).max(0.0));
    }
    ctx.set_fill_color("rgba(251,178,106,0.16)");
    ctx.fill_rect(button_x, field_y, 60.0, field_h);
    ctx.set_fill_color(AMBER);
    ctx.set_font("12px sans-serif");
    ctx.set_text_align(TextAlign::Center);
    fill_clipped(ctx, "BIND", button_x + 30.0, field_y + field_h / 2.0, button_x, field_y, 60.0, field_h);
}
