//! The desktop: a compositor, window manager and shell UI running as one
//! kernel task.
//!
//! ## Layout
//!
//! - A fully transparent **top bar**: the "K" system menu, the active
//!   app's name, and Window/Help menus. Its dropdowns are separate panes
//!   of glass floating over whatever is underneath.
//! - A floating glass **taskbar** at the bottom: Start plus the pinned
//!   apps centred (regular icons, filled when hovered or active, a pill
//!   under running ones), the tray (CPU, memory, clock) on the right.
//! - Glass **windows** in between. Maximizing fills exactly the work area
//!   between the two bars, never sliding under the taskbar.
//!
//! ## Rendering
//!
//! Everything is composed into an off-screen [`Surface`] and only changed
//! regions ("damage") are recomposed and copied to the framebuffer. Each
//! damage rectangle carries the z-level of whatever caused it, so a glass
//! panel only recomputes its blur/refraction when something *beneath* it
//! changed (or it moved) -- a Terminal printing text doesn't make the
//! Terminal's own glass recompute, but it does make a menu floating over
//! it pick up the change. See [`Desktop::resolve_damage`].
//!
//! ## Input
//!
//! Windows are hit-tested front to back with the same squircle SDF they're
//! drawn with, so clicking just outside a rounded corner reaches whatever
//! is behind it.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::Ordering;

use super::apps::{Action, App, AppKind, MouseEvent, Reply, ACCENT, TEXT, TEXT_DIM};
use super::assets;
use super::font;
use super::glass::{self, Glass, GlassStyle, Scratch};
use super::icon_ids as icon;
use super::math::{smoothstep, Spring};
use super::surface::{blend, rgb, Painter, Rect, Surface};
use super::sysmon;
use crate::framebuffer::Canvas;
use crate::{cursor, doom_driver, keyboard, mouse, task, timer};

const TOPBAR_H: i32 = 30;
const TASKBAR_H: i32 = 60;
const TASKBAR_MARGIN: i32 = 8;
const TITLE_H: i32 = 40;
const CELL: i32 = 52;
const CELL_GAP: i32 = 4;
const ITEMS: usize = 1 + AppKind::PINNED.len();

/// Damage z-levels, bottom to top (windows take `L_WIN + z index`).
const L_WIN: u16 = 10;
const L_TOPBAR: u16 = 150;
const L_TASKBAR: u16 = 200;
const L_START: u16 = 300;
const L_MENU: u16 = 310;
const L_TOOLTIP: u16 = 400;
const L_CURSOR: u16 = 1000;
/// Damage that must be recomposed but changed nothing visible (the extra
/// backdrop a recomputing panel samples), so it invalidates nothing.
const L_RENDER_ONLY: u16 = u16::MAX;

/// Ticks between two clicks for them to count as a double-click.
const DOUBLE_CLICK_TICKS: u64 = 40;
/// Hover time before a taskbar tooltip appears.
const TOOLTIP_DELAY: u64 = 45;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
    Min,
    Max,
    Close,
}

struct Window {
    kind: AppKind,
    app: alloc::boxed::Box<dyn App>,
    rect: Rect,
    restore: Option<Rect>,
    minimized: bool,
    glass: Glass,
    open: Spring,
    btn_hover: Option<Btn>,
}

impl Window {
    /// Where the window is drawn this frame: it slides up into place (with
    /// a little spring overshoot) as it opens.
    fn drawn(&self) -> Rect {
        self.rect.offset(0, ((1.0 - self.open.value) * 28.0) as i32)
    }

    fn opacity(&self) -> u8 {
        (smoothstep(0.0, 0.7, self.open.value) * 255.0) as u8
    }

    fn client_size(&self) -> (i32, i32) {
        (self.rect.w, self.rect.h - TITLE_H)
    }

    fn btn_rect(&self, b: Btn) -> Rect {
        let w = self.rect.w;
        match b {
            Btn::Close => Rect::new(w - 60, 6, 40, 28),
            Btn::Max => Rect::new(w - 104, 6, 40, 28),
            Btn::Min => Rect::new(w - 148, 6, 40, 28),
        }
    }

    fn btn_at(&self, lx: i32, ly: i32) -> Option<Btn> {
        [Btn::Min, Btn::Max, Btn::Close].into_iter().find(|&b| self.btn_rect(b).contains(lx, ly))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuId {
    System,
    Window,
    Help,
    Power,
}

#[derive(Clone, Copy)]
enum Cmd {
    Open(AppKind),
    Shell(&'static str),
    Minimize,
    Maximize,
    Close,
}

struct MenuItem {
    /// Empty label = separator.
    label: &'static str,
    cmd: Option<Cmd>,
}

const SEPARATOR: MenuItem = MenuItem { label: "", cmd: None };
const MENU_ITEM_H: i32 = 30;
const MENU_SEP_H: i32 = 11;
const MENU_PAD: i32 = 6;

struct Menu {
    id: MenuId,
    rect: Rect,
    /// Grow from the bottom edge instead of the top (menus that open
    /// upwards, like the Start menu's power menu).
    upward: bool,
    items: Vec<MenuItem>,
    hover: Option<usize>,
    spring: Spring,
    glass: Glass,
}

impl Menu {
    fn drawn(&self) -> Rect {
        let s = 0.9 + 0.1 * self.spring.value;
        let (w, h) = ((self.rect.w as f32 * s) as i32, (self.rect.h as f32 * s) as i32);
        let y = if self.upward { self.rect.bottom() - h } else { self.rect.y };
        Rect::new(self.rect.x, y, w, h)
    }

    fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        if !self.rect.contains(x, y) {
            return None;
        }
        let mut top = self.rect.y + MENU_PAD;
        for (i, item) in self.items.iter().enumerate() {
            let h = if item.label.is_empty() { MENU_SEP_H } else { MENU_ITEM_H };
            if y >= top && y < top + h {
                return item.cmd.is_some().then_some(i);
            }
            top += h;
        }
        None
    }
}

struct StartMenu {
    open: bool,
    spring: Spring,
    glass: Glass,
    /// 0..5 = app tiles, 5 = power button.
    hover: Option<usize>,
}

const START_W: i32 = 436;
const START_H: i32 = 236;
const TILE_W: i32 = 76;
const TILE_H: i32 = 80;

struct Tooltip {
    text: &'static str,
    rect: Rect,
    alpha: f32,
    glass: Glass,
}

#[derive(Clone, Copy)]
enum Layer {
    Win(usize),
    Taskbar,
    Start,
    Menu,
    Tooltip,
}

pub struct Desktop {
    canvas: Canvas,
    bb: Surface,
    wall: Vec<u32>,
    scratch: Scratch,
    windows: Vec<Window>,
    taskbar: Glass,
    start: StartMenu,
    menu: Option<Menu>,
    tooltip: Option<Tooltip>,
    hover_item: Option<usize>,
    hover_since: u64,
    /// Set by a click on a taskbar item: no tooltip again until the
    /// pointer leaves it.
    tooltip_suppressed: bool,
    hover_alpha: [f32; ITEMS],
    topbar_hover: Option<MenuId>,
    damage: Vec<(Rect, u16)>,
    mx: i32,
    my: i32,
    buttons: u8,
    drag: Option<(usize, i32, i32)>,
    last_click: (u64, i32, i32),
    sys_seq: u64,
    clock: String,
    date: String,
    cpu: String,
    ram: String,
    now: u64,
}

impl Desktop {
    fn new(canvas: Canvas) -> Self {
        let (w, h) = (canvas.width() as i32, canvas.height() as i32);
        let (mx, my) = mouse::position();
        Desktop {
            canvas,
            bb: Surface::new(w, h),
            wall: assets::wallpaper(w, h),
            scratch: Scratch::new(((w / 2 + 64) * (h / 2 + 64)) as usize),
            windows: Vec::new(),
            taskbar: Glass::default(),
            start: StartMenu { open: false, spring: Spring::new(0.0), glass: Glass::default(), hover: None },
            menu: None,
            tooltip: None,
            hover_item: None,
            hover_since: 0,
            tooltip_suppressed: false,
            hover_alpha: [0.0; ITEMS],
            topbar_hover: None,
            damage: Vec::new(),
            mx,
            my,
            buttons: 0,
            drag: None,
            last_click: (0, 0, 0),
            sys_seq: u64::MAX,
            clock: String::new(),
            date: String::new(),
            cpu: String::new(),
            ram: String::new(),
            now: timer::ticks(),
        }
    }

    // --- Geometry -------------------------------------------------------

    fn screen(&self) -> Rect {
        self.bb.bounds()
    }

    fn taskbar_rect(&self) -> Rect {
        let s = self.screen();
        Rect::new(TASKBAR_MARGIN, s.h - TASKBAR_MARGIN - TASKBAR_H, s.w - 2 * TASKBAR_MARGIN, TASKBAR_H)
    }

    /// The space windows live in: between the top bar and the taskbar.
    fn work_area(&self) -> Rect {
        let s = self.screen();
        let tb = self.taskbar_rect();
        Rect::new(TASKBAR_MARGIN, TOPBAR_H + 4, s.w - 2 * TASKBAR_MARGIN, tb.y - 8 - (TOPBAR_H + 4))
    }

    fn item_rect(&self, i: usize) -> Rect {
        let tb = self.taskbar_rect();
        let total = ITEMS as i32 * CELL + (ITEMS as i32 - 1) * CELL_GAP;
        let x0 = self.screen().w / 2 - total / 2;
        Rect::new(x0 + i as i32 * (CELL + CELL_GAP), tb.y + (TASKBAR_H - CELL) / 2, CELL, CELL)
    }

    fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        (0..ITEMS).find(|&i| self.item_rect(i).contains(x, y))
    }

    fn tray_rect(&self) -> Rect {
        let tb = self.taskbar_rect();
        Rect::new(tb.right() - 300, tb.y, 300, tb.h)
    }

    fn start_rect(&self) -> Rect {
        let tb = self.taskbar_rect();
        Rect::new((self.screen().w - START_W) / 2, tb.y - 12 - START_H, START_W, START_H)
    }

    fn start_drawn(&self) -> Rect {
        let r = self.start_rect();
        let s = 0.75 + 0.25 * self.start.spring.value;
        let (w, h) = ((r.w as f32 * s) as i32, (r.h as f32 * s) as i32);
        Rect::new(r.x + (r.w - w) / 2, r.bottom() - h, w, h)
    }

    fn start_visible(&self) -> bool {
        self.start.open
    }

    fn tile_rect(i: usize) -> Rect {
        let x0 = (START_W - 5 * TILE_W - 4 * 6) / 2;
        Rect::new(x0 + i as i32 * (TILE_W + 6), 48, TILE_W, TILE_H)
    }

    fn power_rect() -> Rect {
        Rect::new(START_W - 58, START_H - 50, 42, 38)
    }

    fn start_hit(&self, x: i32, y: i32) -> Option<usize> {
        let r = self.start_rect();
        let (lx, ly) = (x - r.x, y - r.y);
        if Self::power_rect().contains(lx, ly) {
            return Some(5);
        }
        (0..5).find(|&i| Self::tile_rect(i).contains(lx, ly))
    }

    fn active(&self) -> Option<usize> {
        self.windows.iter().rposition(|w| !w.minimized)
    }

    fn active_kind(&self) -> Option<AppKind> {
        self.active().map(|i| self.windows[i].kind)
    }

    fn topbar_items(&self) -> [(MenuId, Rect); 3] {
        let name = self.active_kind().map_or("KonjacOS", |k| k.name());
        let name_end = 52 + font::UI_BOLD.width(name);
        let ww = font::UI.width("Window");
        let hw = font::UI.width("Help");
        [
            (MenuId::System, Rect::new(8, 3, 36, 24)),
            (MenuId::Window, Rect::new(name_end + 8, 3, ww + 20, 24)),
            (MenuId::Help, Rect::new(name_end + 8 + ww + 24, 3, hw + 20, 24)),
        ]
    }

    fn topbar_item_at(&self, x: i32, y: i32) -> Option<MenuId> {
        self.topbar_items().iter().find(|(_, r)| r.contains(x, y)).map(|&(id, _)| id)
    }

    fn window_at(&self, x: i32, y: i32) -> Option<usize> {
        (0..self.windows.len()).rev().find(|&i| {
            let w = &self.windows[i];
            let d = w.drawn();
            !w.minimized && d.contains(x, y) && w.glass.shape.hit(x - d.x, y - d.y)
        })
    }

    // --- Damage -----------------------------------------------------------

    fn damage(&mut self, r: Rect, level: u16) {
        if !r.is_empty() {
            self.damage.push((r, level));
        }
    }

    fn damage_window(&mut self, i: usize) {
        let r = self.windows[i].drawn().expand(glass::WINDOW.reach());
        self.damage(r, L_WIN + i as u16);
    }

    fn damage_client(&mut self, i: usize) {
        let d = self.windows[i].drawn();
        self.damage(Rect::new(d.x, d.y + TITLE_H, d.w, d.h - TITLE_H), L_WIN + i as u16);
    }

    fn damage_cursor(&mut self) {
        let r = Rect::new(self.mx - cursor::HOTSPOT_X, self.my - cursor::HOTSPOT_Y, cursor::WIDTH as i32, cursor::HEIGHT as i32);
        self.damage(r, L_CURSOR);
    }

    fn glass_layers(&self) -> Vec<(Layer, u16, Rect, &'static GlassStyle)> {
        let mut v = Vec::new();
        for (i, w) in self.windows.iter().enumerate() {
            if !w.minimized {
                v.push((Layer::Win(i), L_WIN + i as u16, w.drawn(), &glass::WINDOW));
            }
        }
        v.push((Layer::Taskbar, L_TASKBAR, self.taskbar_rect(), &glass::TASKBAR));
        if self.start_visible() {
            v.push((Layer::Start, L_START, self.start_drawn(), &glass::MENU));
        }
        if let Some(m) = &self.menu {
            v.push((Layer::Menu, L_MENU, m.drawn(), &glass::MENU));
        }
        if let Some(t) = &self.tooltip {
            v.push((Layer::Tooltip, L_TOOLTIP, t.rect, &glass::TOOLTIP));
        }
        v
    }

    fn glass_mut(&mut self, layer: Layer) -> &mut Glass {
        match layer {
            Layer::Win(i) => &mut self.windows[i].glass,
            Layer::Taskbar => &mut self.taskbar,
            Layer::Start => &mut self.start.glass,
            Layer::Menu => &mut self.menu.as_mut().unwrap().glass,
            Layer::Tooltip => &mut self.tooltip.as_mut().unwrap().glass,
        }
    }

    /// Decides which glass panels must recompute this frame -- those that
    /// moved, and those with damage from a *lower* layer under them -- and
    /// widens the damage so each one's whole backdrop gets recomposed
    /// before it's sampled. Bottom-up, so a recomputed panel in turn
    /// invalidates the panels above it.
    fn resolve_damage(&mut self) {
        for (layer, level, rect, style) in self.glass_layers() {
            let region = rect.expand(style.pad());
            let stale = self.glass_mut(layer).needs_compute(rect)
                || self.damage.iter().any(|(r, l)| *l < level && r.intersects(&region));
            if stale {
                self.glass_mut(layer).invalidate();
                self.damage.push((rect.expand(style.reach()), level));
                self.damage.push((region, L_RENDER_ONLY));
            }
        }
    }

    fn flush(&mut self) {
        if self.damage.is_empty() {
            return;
        }
        self.resolve_damage();
        let screen = self.screen();
        let mut rects: Vec<Rect> = self.damage.drain(..).map(|(r, _)| r.intersect(&screen)).filter(|r| !r.is_empty()).collect();
        // Merge overlapping (or nearly touching) rectangles, so every
        // panel's backdrop region lands inside a single one.
        'merge: loop {
            for i in 0..rects.len() {
                for j in i + 1..rects.len() {
                    if rects[i].expand(8).intersects(&rects[j]) {
                        rects[i] = rects[i].union(&rects[j]);
                        rects.swap_remove(j);
                        continue 'merge;
                    }
                }
            }
            break;
        }
        for r in rects {
            self.render(r);
            self.canvas.present(&self.bb.px, self.bb.w as usize, r.x as usize, r.y as usize, r.w as usize, r.h as usize);
        }
    }

    // --- Composition ------------------------------------------------------

    fn render(&mut self, clip: Rect) {
        let active = self.active();
        let topbar = self.topbar_items();
        let active_name = self.active_kind().map_or("KonjacOS", |k| k.name());
        let tb = self.taskbar_rect();
        let items: Vec<Rect> = (0..ITEMS).map(|i| self.item_rect(i)).collect();
        let running: Vec<bool> = AppKind::PINNED.iter().map(|k| self.windows.iter().any(|w| w.kind == *k)).collect();
        let active_kind = self.active_kind();
        let start_drawn = self.start_drawn();
        let start_open = self.start_visible();

        // Occlusion: if a window's opaque content (DOOM's frame) covers
        // this whole region, nothing beneath it can show -- start there.
        let first = self.windows.iter().enumerate().rev().find_map(|(i, w)| {
            if w.minimized || w.opacity() < 255 {
                return None;
            }
            let d = w.drawn();
            let (cw, ch) = w.client_size();
            let r = w.app.opaque_rect(cw, ch)?.offset(d.x, d.y + TITLE_H);
            (r.intersect(&clip) == clip).then_some(i)
        });

        let Desktop { bb, wall, scratch, windows, taskbar, start, menu, tooltip, hover_alpha, topbar_hover, mx, my, clock, date, cpu, ram, .. } = self;
        let (sw, sh) = (bb.w, bb.h);

        if first.is_none() {
            bb.painter(clip).blit(0, 0, sw, sh, wall, sw);
        }

        for (i, w) in windows.iter_mut().enumerate() {
            if w.minimized || first.is_some_and(|f| i < f) {
                continue;
            }
            if first == Some(i) {
                // Fully covered by its own opaque content: just repaint that.
                let d = w.drawn();
                let (cw, ch) = (d.w, d.h - TITLE_H);
                let mut p = bb.painter(clip);
                let mut cp = p.sub(Rect::new(d.x, d.y + TITLE_H, cw, ch));
                w.app.paint(&mut cp, cw, ch);
                continue;
            }
            let d = w.drawn();
            if !d.expand(glass::WINDOW.pad()).intersects(&clip) {
                continue;
            }
            let op = w.opacity();
            w.glass.render(bb, d, &glass::WINDOW, clip, op, scratch);
            let mut p = bb.painter(clip);
            p.alpha = op;
            let mut wp = p.sub(d);
            paint_chrome(&mut wp, w, Some(i) == active);
            let (cw, ch) = (d.w, d.h - TITLE_H);
            let mut cp = wp.sub(Rect::new(0, TITLE_H, cw, ch));
            w.app.paint(&mut cp, cw, ch);
        }

        paint_topbar(&mut bb.painter(clip), &topbar, active_name, *topbar_hover, menu.as_ref().map(|m| m.id));

        taskbar.render(bb, tb, &glass::TASKBAR, clip, 255, scratch);
        {
            let mut p = bb.painter(clip);
            for (i, r) in items.iter().enumerate() {
                let h = if i == 0 && start_open { 1.0 } else { hover_alpha[i] };
                if h > 0.0 {
                    p.fill_squircle(r.expand(-2), 10.0, TEXT, (h * 0.15 * 255.0) as u8);
                }
                if i == 0 {
                    let (lw, lh, m) = assets::logo_small();
                    p.draw_mask(r.x + (r.w - lw) / 2, r.y + (r.h - lh) / 2, lw, lh, m, TEXT, 255);
                    continue;
                }
                let kind = AppKind::PINNED[i - 1];
                let is_active = active_kind == Some(kind);
                let (regular, filled) = kind.taskbar_icons();
                let id = if is_active || h > 0.5 { filled } else { regular };
                let (iw, ih, m) = assets::icon(id);
                p.draw_mask(r.x + (r.w - iw) / 2, r.y + (r.h - ih) / 2 - 2, iw, ih, m, TEXT, 255);
                if running[i - 1] {
                    let pw = if is_active { 18 } else { 6 };
                    p.fill_squircle(Rect::new(r.x + (r.w - pw) / 2, r.bottom() - 5, pw, 3), 1.5, TEXT, if is_active { 235 } else { 150 });
                }
            }
            paint_tray(&mut p, tb, clock, date, cpu, ram);
        }

        if start_open {
            let op = (smoothstep(0.0, 0.5, start.spring.value) * 255.0) as u8;
            start.glass.render(bb, start_drawn, &glass::MENU, clip, op, scratch);
            let content = (smoothstep(0.8, 1.0, start.spring.value) * 255.0) as u8;
            if content > 0 {
                let mut p = bb.painter(clip);
                p.alpha = content;
                let mut sp = p.sub(Rect::new(start_drawn.x, start_drawn.y, START_W, START_H));
                paint_start(&mut sp, start.hover);
            }
        }

        if let Some(m) = menu {
            let d = m.drawn();
            let op = (smoothstep(0.0, 0.5, m.spring.value) * 255.0) as u8;
            m.glass.render(bb, d, &glass::MENU, clip, op, scratch);
            let mut p = bb.painter(clip);
            p.alpha = (smoothstep(0.6, 1.0, m.spring.value) * 255.0) as u8;
            let mut mp = p.sub(d);
            paint_menu(&mut mp, m);
        }

        if let Some(t) = tooltip {
            let op = (t.alpha * 255.0) as u8;
            t.glass.render(bb, t.rect, &glass::TOOLTIP, clip, op, scratch);
            let mut p = bb.painter(clip);
            p.alpha = op;
            let tx = t.rect.x + (t.rect.w - font::UI.width(t.text)) / 2;
            p.text_shadowed(&font::UI, tx, t.rect.y + 7, t.text, TEXT, 255);
        }

        paint_cursor(bb, clip, *mx, *my);
    }

    // --- Windows ----------------------------------------------------------

    fn open_app(&mut self, kind: AppKind) {
        if let Some(i) = self.windows.iter().position(|w| w.kind == kind) {
            if self.windows[i].minimized {
                self.windows[i].minimized = false;
            }
            self.focus(i);
            return;
        }
        let mut app = kind.create();
        let (cw, ch) = app.client_size();
        let wa = self.work_area();
        let (w, h) = (cw.min(wa.w), (ch + TITLE_H).min(wa.h));
        let cascade = self.windows.len() as i32 * 32;
        let x = (wa.x + (wa.w - w) / 2 + cascade - 48).clamp(wa.x, wa.right() - w);
        let y = (wa.y + (wa.h - h) / 2 + cascade - 32).clamp(wa.y, wa.bottom() - h);
        app.resized(w, h - TITLE_H);
        let mut open = Spring::new(0.0);
        open.target = 1.0;
        self.windows.push(Window { kind, app, rect: Rect::new(x, y, w, h), restore: None, minimized: false, glass: Glass::default(), open, btn_hover: None });
        let i = self.windows.len() - 1;
        self.damage_window(i);
        self.focus_changed();
    }

    /// Raises window `i` to the top; returns its new index.
    fn focus(&mut self, i: usize) -> usize {
        let top = self.windows.len() - 1;
        if i != top {
            let w = self.windows.remove(i);
            self.windows.push(w);
            // Level L_WIN: every window now above its old position has a
            // new backdrop.
            let r = self.windows[top].drawn().expand(glass::WINDOW.reach());
            self.damage(r, L_WIN);
        } else {
            self.damage_window(top);
        }
        self.focus_changed();
        top
    }

    fn focus_changed(&mut self) {
        keyboard::set_doom_focus(self.active_kind() == Some(AppKind::Doom));
        let s = self.screen();
        self.damage(Rect::new(0, 0, s.w, TOPBAR_H), L_TOPBAR);
        for i in 0..ITEMS {
            let r = self.item_rect(i);
            self.damage(r, L_TASKBAR);
        }
    }

    fn close_window(&mut self, i: usize) {
        self.damage_window(i);
        let mut w = self.windows.remove(i);
        w.app.closed();
        if self.drag.is_some_and(|(d, ..)| d == i) {
            self.drag = None;
        }
        self.focus_changed();
    }

    fn minimize(&mut self, i: usize) {
        self.damage_window(i);
        self.windows[i].minimized = true;
        self.focus_changed();
    }

    fn toggle_maximize(&mut self, i: usize) {
        self.damage_window(i);
        let wa = self.work_area();
        let w = &mut self.windows[i];
        w.rect = match w.restore.take() {
            Some(r) => r,
            None => {
                w.restore = Some(w.rect);
                wa
            }
        };
        let (cw, ch) = w.client_size();
        w.app.resized(cw, ch);
        self.damage_window(i);
    }

    fn move_window(&mut self, i: usize, x: i32, y: i32) {
        let wa = self.work_area();
        let s = self.screen();
        let w = &self.windows[i];
        let x = x.clamp(80 - w.rect.w, s.w - 80);
        let y = y.clamp(wa.y, wa.bottom() - TITLE_H);
        if x == w.rect.x && y == w.rect.y {
            return;
        }
        self.damage_window(i);
        self.windows[i].rect.x = x;
        self.windows[i].rect.y = y;
    }

    /// Taskbar click: launch, restore, focus or minimize.
    fn taskbar_click(&mut self, kind: AppKind) {
        if kind == AppKind::Doom && !self.windows.iter().any(|w| w.kind == kind) {
            match crate::commands::launch_doom() {
                Ok(_) => self.open_app(kind),
                Err(e) => {
                    crate::println!("doom: {e}");
                    self.open_app(AppKind::Terminal);
                }
            }
            return;
        }
        match self.windows.iter().position(|w| w.kind == kind) {
            Some(i) if self.windows[i].minimized => {
                self.windows[i].minimized = false;
                self.focus(i);
            }
            Some(i) if Some(i) == self.active() => self.minimize(i),
            Some(i) => {
                self.focus(i);
            }
            None => self.open_app(kind),
        }
    }

    fn shell_command(&mut self, cmd: &str) {
        keyboard::inject(cmd);
        self.open_app(AppKind::Terminal);
    }

    fn apply_reply(&mut self, i: usize, reply: Reply) {
        if reply.repaint {
            self.damage_client(i);
        }
        if let Some(Action::Shell(cmd)) = reply.action {
            self.shell_command(&cmd);
        }
    }

    fn run_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Open(kind) => self.open_app(kind),
            Cmd::Shell(s) => self.shell_command(s),
            Cmd::Minimize => {
                if let Some(i) = self.active() {
                    self.minimize(i);
                }
            }
            Cmd::Maximize => {
                if let Some(i) = self.active() {
                    self.toggle_maximize(i);
                }
            }
            Cmd::Close => {
                if let Some(i) = self.active() {
                    self.close_window(i);
                }
            }
        }
    }

    // --- Menus ------------------------------------------------------------

    fn open_menu(&mut self, id: MenuId) {
        self.close_menu();
        let maximized = self.active().is_some_and(|i| self.windows[i].restore.is_some());
        let items = match id {
            MenuId::System => alloc::vec![
                MenuItem { label: "About KonjacOS", cmd: Some(Cmd::Open(AppKind::About)) },
                MenuItem { label: "System Monitor", cmd: Some(Cmd::Open(AppKind::Monitor)) },
                SEPARATOR,
                MenuItem { label: "Restart...", cmd: Some(Cmd::Shell("reboot\n")) },
                MenuItem { label: "Shut Down...", cmd: Some(Cmd::Shell("halt\n")) },
            ],
            MenuId::Window => alloc::vec![
                MenuItem { label: "Minimize", cmd: Some(Cmd::Minimize) },
                MenuItem { label: if maximized { "Restore" } else { "Maximize" }, cmd: Some(Cmd::Maximize) },
                SEPARATOR,
                MenuItem { label: "Close", cmd: Some(Cmd::Close) },
            ],
            MenuId::Help => alloc::vec![
                MenuItem { label: "Shell Commands", cmd: Some(Cmd::Shell("help\n")) },
                MenuItem { label: "About KonjacOS", cmd: Some(Cmd::Open(AppKind::About)) },
            ],
            MenuId::Power => alloc::vec![
                MenuItem { label: "Restart", cmd: Some(Cmd::Shell("reboot\n")) },
                MenuItem { label: "Shut Down", cmd: Some(Cmd::Shell("halt\n")) },
            ],
        };
        let width = items.iter().map(|it| font::UI.width(it.label)).max().unwrap_or(0) + 56;
        let height = 2 * MENU_PAD + items.iter().map(|it| if it.label.is_empty() { MENU_SEP_H } else { MENU_ITEM_H }).sum::<i32>();
        let (rect, upward) = match id {
            MenuId::Power => {
                let s = self.start_rect();
                let p = Self::power_rect();
                (Rect::new(s.x + p.right() - width, s.y + p.y - 8 - height, width, height), true)
            }
            _ => {
                let anchor = self.topbar_items().iter().find(|(m, _)| *m == id).map_or(Rect::default(), |&(_, r)| r);
                (Rect::new(anchor.x, TOPBAR_H + 4, width, height), false)
            }
        };
        let mut spring = Spring::new(0.0);
        spring.target = 1.0;
        self.menu = Some(Menu { id, rect, upward, items, hover: None, spring, glass: Glass::default() });
        self.damage(rect.expand(glass::MENU.reach()), L_MENU);
        self.damage(Rect::new(0, 0, self.screen().w, TOPBAR_H), L_TOPBAR);
    }

    fn close_menu(&mut self) {
        if let Some(m) = self.menu.take() {
            self.damage(m.rect.expand(glass::MENU.reach()), L_MENU);
            self.damage(Rect::new(0, 0, self.screen().w, TOPBAR_H), L_TOPBAR);
        }
    }

    fn open_start(&mut self) {
        self.hide_tooltip();
        self.start.open = true;
        self.start.spring.snap(0.0);
        self.start.spring.target = 1.0;
        self.start.hover = None;
        self.damage(self.start_rect().expand(glass::MENU.reach()), L_START);
        self.damage(self.item_rect(0), L_TASKBAR);
    }

    fn close_start(&mut self) {
        if self.start.open {
            self.start.open = false;
            self.damage(self.start_rect().expand(glass::MENU.reach()), L_START);
            self.damage(self.item_rect(0), L_TASKBAR);
        }
    }

    fn hide_tooltip(&mut self) {
        if let Some(t) = self.tooltip.take() {
            self.damage(t.rect.expand(glass::TOOLTIP.reach()), L_TOOLTIP);
        }
    }

    // --- Input ------------------------------------------------------------

    fn poll_input(&mut self) {
        let (x, y) = mouse::position();
        let b = mouse::buttons();
        if (x, y) != (self.mx, self.my) {
            self.damage_cursor();
            self.mx = x;
            self.my = y;
            self.damage_cursor();
            self.on_move();
        }
        let left = b & mouse::LEFT_BUTTON != 0;
        let was = self.buttons & mouse::LEFT_BUTTON != 0;
        self.buttons = b;
        if left && !was {
            self.on_press();
        } else if !left && was {
            self.drag = None;
        }
    }

    fn on_move(&mut self) {
        let (x, y) = (self.mx, self.my);
        if let Some((i, ox, oy)) = self.drag {
            if let Some(w) = self.windows.get(i) {
                if w.restore.is_some() {
                    // Dragging a maximized window restores it under the
                    // cursor, like other desktops do.
                    let r = w.restore.unwrap();
                    let ratio = ox as f32 / w.rect.w as f32;
                    self.damage_window(i);
                    let w = &mut self.windows[i];
                    w.restore = None;
                    let nox = (r.w as f32 * ratio) as i32;
                    w.rect = Rect::new(x - nox, y - oy, r.w, r.h);
                    let (cw, ch) = w.client_size();
                    w.app.resized(cw, ch);
                    self.drag = Some((i, nox, oy));
                } else {
                    self.move_window(i, x - ox, y - oy);
                }
            }
            return;
        }

        let item = if self.taskbar_rect().contains(x, y) { self.item_at(x, y) } else { None };
        if item != self.hover_item {
            self.hover_item = item;
            self.hover_since = self.now;
            self.tooltip_suppressed = false;
            self.hide_tooltip();
        }

        let top = if y < TOPBAR_H { self.topbar_item_at(x, y) } else { None };
        if top != self.topbar_hover {
            self.topbar_hover = top;
            let w = self.screen().w;
            self.damage(Rect::new(0, 0, w, TOPBAR_H), L_TOPBAR);
        }

        if let Some(m) = &mut self.menu {
            let h = m.item_at(x, y);
            if h != m.hover {
                m.hover = h;
                let r = m.rect;
                self.damage(r, L_MENU);
            }
        }

        if self.start.open {
            let h = self.start_hit(x, y);
            if h != self.start.hover {
                self.start.hover = h;
                let r = self.start_rect();
                self.damage(r, L_START);
            }
        }

        let over = if self.menu.is_none() && !self.start.open && y >= TOPBAR_H { self.window_at(x, y) } else { None };
        for i in 0..self.windows.len() {
            let w = &self.windows[i];
            let d = w.drawn();
            let hover = if over == Some(i) { w.btn_at(x - d.x, y - d.y) } else { None };
            if hover != w.btn_hover {
                self.windows[i].btn_hover = hover;
                self.damage(Rect::new(d.x, d.y, d.w, TITLE_H), L_WIN + i as u16);
            }
        }
        if let Some(i) = over {
            let d = self.windows[i].drawn();
            let (cw, ch) = self.windows[i].client_size();
            if y - d.y >= TITLE_H {
                let reply = self.windows[i].app.mouse(MouseEvent::Move, x - d.x, y - d.y - TITLE_H, cw, ch);
                self.apply_reply(i, reply);
            }
        }
    }

    fn on_press(&mut self) {
        let (x, y) = (self.mx, self.my);
        let (lt, lx, ly) = self.last_click;
        let double = self.now - lt <= DOUBLE_CLICK_TICKS && (x - lx).abs() < 6 && (y - ly).abs() < 6;
        self.last_click = if double { (0, x, y) } else { (self.now, x, y) };

        if let Some(m) = &self.menu {
            let (id, rect, hit) = (m.id, m.rect, m.item_at(x, y));
            if rect.contains(x, y) {
                if let Some(cmd) = hit.and_then(|i| self.menu.as_ref().unwrap().items[i].cmd) {
                    self.close_menu();
                    if id == MenuId::Power {
                        self.close_start();
                    }
                    self.run_cmd(cmd);
                }
                return;
            }
            self.close_menu();
            if let Some(other) = self.topbar_item_at(x, y).filter(|&o| o != id) {
                self.open_menu(other);
            }
            if !(self.start.open && self.start_rect().contains(x, y)) {
                return;
            }
        }

        if self.start.open {
            if self.start_rect().contains(x, y) {
                match self.start_hit(x, y) {
                    Some(5) => self.open_menu(MenuId::Power),
                    Some(i) => {
                        self.close_start();
                        self.taskbar_click_or_open(AppKind::PINNED[i]);
                    }
                    None => {}
                }
                return;
            }
            self.close_start();
            return;
        }

        if self.taskbar_rect().contains(x, y) {
            self.hide_tooltip();
            self.tooltip_suppressed = true;
            match self.item_at(x, y) {
                Some(0) => self.open_start(),
                Some(i) => self.taskbar_click(AppKind::PINNED[i - 1]),
                None => {}
            }
            return;
        }

        if y < TOPBAR_H {
            if let Some(id) = self.topbar_item_at(x, y) {
                self.open_menu(id);
            }
            return;
        }

        if let Some(i) = self.window_at(x, y) {
            let i = self.focus(i);
            let d = self.windows[i].drawn();
            let (lx, ly) = (x - d.x, y - d.y);
            if ly < TITLE_H {
                match self.windows[i].btn_at(lx, ly) {
                    Some(Btn::Close) => self.close_window(i),
                    Some(Btn::Max) => self.toggle_maximize(i),
                    Some(Btn::Min) => self.minimize(i),
                    None if double => self.toggle_maximize(i),
                    None => self.drag = Some((i, lx, ly)),
                }
            } else {
                let (cw, ch) = self.windows[i].client_size();
                let ev = if double { MouseEvent::DoubleClick } else { MouseEvent::Down };
                let reply = self.windows[i].app.mouse(ev, lx, ly - TITLE_H, cw, ch);
                self.apply_reply(i, reply);
            }
        }
    }

    /// Start menu tiles launch (or bring forward) an app, never minimize.
    fn taskbar_click_or_open(&mut self, kind: AppKind) {
        if kind == AppKind::Doom {
            self.taskbar_click(kind);
        } else {
            self.open_app(kind);
        }
    }

    // --- Per-frame updates ------------------------------------------------

    fn poll_sources(&mut self) {
        for i in 0..self.windows.len() {
            if let Some(r) = self.windows[i].app.tick() {
                if !self.windows[i].minimized {
                    let d = self.windows[i].drawn();
                    let client = Rect::new(d.x, d.y + TITLE_H, d.w, d.h - TITLE_H);
                    self.damage(r.offset(client.x, client.y).intersect(&client), L_WIN + i as u16);
                }
            }
        }

        let seq = sysmon::SEQ.load(Ordering::Acquire);
        if seq != self.sys_seq {
            self.sys_seq = seq;
            let s = sysmon::latest();
            let (mut clock, mut date, mut cpu, mut ram) = (String::new(), String::new(), String::new(), String::new());
            let t = s.time;
            let h12 = if t.hour % 12 == 0 { 12 } else { t.hour % 12 };
            let _ = write!(clock, "{}:{:02} {}", h12, t.minute, if t.hour < 12 { "AM" } else { "PM" });
            let _ = write!(date, "{}/{}/{}", t.month, t.day, t.year);
            let _ = write!(cpu, "{}%", s.cpu);
            let pct = if s.mem_total > 0 { s.mem_used * 100 / s.mem_total } else { 0 };
            let _ = write!(ram, "{pct}%");
            if (&clock, &date, &cpu, &ram) != (&self.clock, &self.date, &self.cpu, &self.ram) {
                self.clock = clock;
                self.date = date;
                self.cpu = cpu;
                self.ram = ram;
                let r = self.tray_rect();
                self.damage(r, L_TASKBAR);
            }
        }

        // DOOM's window follows its task: opened when it starts (from the
        // taskbar or the `doom` shell command), closed when it ends.
        if self.now % 20 == 0 {
            let running = doom_driver::running_task().is_some();
            let window = self.windows.iter().position(|w| w.kind == AppKind::Doom);
            match (running, window) {
                (true, None) => self.open_app(AppKind::Doom),
                (false, Some(i)) => {
                    self.damage_window(i);
                    self.windows.remove(i);
                    self.focus_changed();
                }
                _ => {}
            }
        }
    }

    fn animate(&mut self) {
        for i in 0..self.windows.len() {
            if !self.windows[i].open.settled() {
                self.damage_window(i);
                self.windows[i].open.step(320.0, 21.0);
                if self.windows[i].open.settled() {
                    self.windows[i].open.snap(1.0);
                }
                self.damage_window(i);
            }
        }

        // Hover boxes fade in/out linearly over 80ms (8 ticks).
        for i in 0..ITEMS {
            let target = if self.hover_item == Some(i) { 1.0 } else { 0.0 };
            let cur = self.hover_alpha[i];
            if cur != target {
                self.hover_alpha[i] = if target > cur { (cur + 0.125).min(1.0) } else { (cur - 0.125).max(0.0) };
                let r = self.item_rect(i);
                self.damage(r, L_TASKBAR);
            }
        }

        match self.hover_item {
            Some(i) if self.tooltip.is_none() && !self.tooltip_suppressed && !self.start.open && self.menu.is_none() && self.now - self.hover_since > TOOLTIP_DELAY && self.buttons == 0 => {
                let text = if i == 0 { "Start" } else { AppKind::PINNED[i - 1].name() };
                let r = self.item_rect(i);
                let w = font::UI.width(text) + 28;
                let rect = Rect::new(r.x + (r.w - w) / 2, self.taskbar_rect().y - 42, w, 32);
                self.tooltip = Some(Tooltip { text, rect, alpha: 0.0, glass: Glass::default() });
                self.damage(rect.expand(glass::TOOLTIP.reach()), L_TOOLTIP);
            }
            _ => {}
        }
        if let Some(t) = &mut self.tooltip {
            if t.alpha < 1.0 {
                t.alpha = (t.alpha + 0.15).min(1.0);
                let r = t.rect;
                self.damage(r, L_TOOLTIP);
            }
        }

        if self.start.open && !self.start.spring.settled() {
            let before = self.start_drawn().expand(glass::MENU.reach());
            self.start.spring.step(260.0, 17.0);
            if self.start.spring.settled() {
                self.start.spring.snap(1.0);
            }
            self.damage(before, L_START);
        }

        if let Some(m) = &mut self.menu {
            if !m.spring.settled() {
                let before = m.drawn().expand(glass::MENU.reach());
                m.spring.step(300.0, 20.0);
                if m.spring.settled() {
                    m.spring.snap(1.0);
                }
                self.damage(before, L_MENU);
            }
        }
    }

    // --- Boot ---------------------------------------------------------------

    /// Picks up from the "K" logo the kernel drew at boot: a short loading
    /// bar under it, then a cross-fade into the desktop.
    fn boot(&mut self) {
        let s = self.screen();
        let (lw, lh, logo) = assets::logo();
        let (lx, ly) = ((s.w - lw) / 2, (s.h - lh) / 2);
        let track = Rect::new(s.w / 2 - 80, ly + lh + 56, 160, 4);
        {
            let mut p = self.bb.painter(s);
            p.fill_rect(s, 0, 255);
            p.draw_mask(lx, ly, lw, lh, logo, TEXT, 255);
        }

        let start = timer::ticks();
        const LOAD: u64 = 110;
        loop {
            let t = ((timer::ticks() - start) as f32 / LOAD as f32).min(1.0);
            let eased = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
            {
                let mut p = self.bb.painter(track);
                p.fill_rect(track, 0, 255);
                p.fill_squircle(track, 2.0, TEXT, 50);
                p.fill_squircle(Rect::new(track.x, track.y, ((track.w as f32 * eased) as i32).max(4), track.h), 2.0, TEXT, 255);
            }
            self.canvas.present(&self.bb.px, s.w as usize, track.x as usize, track.y as usize, track.w as usize, track.h as usize);
            if t >= 1.0 {
                break;
            }
            task::sleep_ticks(1);
        }
        let splash: Vec<u32> = self.bb.px.clone();

        // Compose the first desktop frame, then fade it in over the splash.
        self.poll_sources();
        for w in &mut self.windows {
            w.open.snap(1.0);
        }
        self.damage.clear();
        self.render(s);
        let desk: Vec<u32> = self.bb.px.clone();
        const FADE: u64 = 40;
        let start = timer::ticks();
        loop {
            let t = ((timer::ticks() - start) as f32 / FADE as f32).min(1.0);
            let a = (smoothstep(0.0, 1.0, t) * 255.0) as u32;
            for (out, (&from, &to)) in self.bb.px.iter_mut().zip(splash.iter().zip(desk.iter())) {
                *out = blend(from, to, a);
            }
            self.canvas.present(&self.bb.px, s.w as usize, 0, 0, s.w as usize, s.h as usize);
            if t >= 1.0 {
                break;
            }
            task::sleep_ticks(1);
        }
        self.bb.px.copy_from_slice(&desk);
        self.canvas.present(&self.bb.px, s.w as usize, 0, 0, s.w as usize, s.h as usize);
    }
}

// --- Painting helpers -----------------------------------------------------

fn paint_chrome(p: &mut Painter, w: &Window, active: bool) {
    let a = if active { 255 } else { 165 };
    let (iw, ih, m) = assets::icon(w.kind.small_icon());
    p.draw_mask(22, (TITLE_H - ih) / 2, iw, ih, m, TEXT, a);
    let title = w.kind.name();
    p.text_shadowed(&font::UI_BOLD, 52, (TITLE_H - font::UI_BOLD.line_height()) / 2, title, TEXT, a);
    for b in [Btn::Min, Btn::Max, Btn::Close] {
        let r = w.btn_rect(b);
        if w.btn_hover == Some(b) {
            if b == Btn::Close {
                p.fill_squircle(r, 9.0, rgb(232, 37, 52), 225);
            } else {
                p.fill_squircle(r, 9.0, TEXT, 34);
            }
        }
        let id = match b {
            Btn::Min => icon::MINIMIZE_16,
            Btn::Max if w.restore.is_some() => icon::RESTORE_16,
            Btn::Max => icon::MAXIMIZE_16,
            Btn::Close => icon::CLOSE_16,
        };
        let (iw, ih, m) = assets::icon(id);
        p.draw_mask(r.x + (r.w - iw) / 2, r.y + (r.h - ih) / 2, iw, ih, m, TEXT, a);
    }
}

fn paint_topbar(p: &mut Painter, items: &[(MenuId, Rect); 3], name: &str, hover: Option<MenuId>, open: Option<MenuId>) {
    for &(id, r) in items {
        if hover == Some(id) || open == Some(id) {
            p.fill_squircle(r, 8.0, TEXT, if open == Some(id) { 46 } else { 30 });
        }
    }
    let sys = items[0].1;
    let (lw, lh, m) = assets::logo_small();
    p.draw_mask(sys.x + (sys.w - lw) / 2 + 1, sys.y + (sys.h - lh) / 2 + 1, lw, lh, m, 0, 70);
    p.draw_mask(sys.x + (sys.w - lw) / 2, sys.y + (sys.h - lh) / 2, lw, lh, m, TEXT, 255);
    let ty = (TOPBAR_H - font::UI.line_height()) / 2;
    p.text_shadowed(&font::UI_BOLD, 52, ty, name, TEXT, 255);
    p.text_shadowed(&font::UI, items[1].1.x + 10, ty, "Window", TEXT, 255);
    p.text_shadowed(&font::UI, items[2].1.x + 10, ty, "Help", TEXT, 255);
}

fn paint_tray(p: &mut Painter, tb: Rect, clock: &str, date: &str, cpu: &str, ram: &str) {
    let right = tb.right() - 20;
    let cw = font::UI.width(clock).max(font::SMALL.width(date));
    p.text_shadowed(&font::UI, right - font::UI.width(clock), tb.y + 12, clock, TEXT, 255);
    p.text_shadowed(&font::SMALL, right - font::SMALL.width(date), tb.y + 32, date, TEXT_DIM, 255);
    let mut x = right - cw - 26;
    for (id, text) in [(icon::RAM_20, ram), (icon::CPU_20, cpu)] {
        let tw = font::UI.width(text);
        x -= tw;
        p.text_shadowed(&font::UI, x, tb.y + (TASKBAR_H - font::UI.line_height()) / 2, text, TEXT, 255);
        let (iw, ih, m) = assets::icon(id);
        x -= iw + 6;
        p.draw_mask(x, tb.y + (TASKBAR_H - ih) / 2, iw, ih, m, TEXT, 230);
        x -= 18;
    }
}

fn paint_start(p: &mut Painter, hover: Option<usize>) {
    p.text_shadowed(&font::UI_BOLD, 24, 18, "Pinned", TEXT, 255);
    for (i, kind) in AppKind::PINNED.iter().enumerate() {
        let r = Desktop::tile_rect(i);
        if hover == Some(i) {
            p.fill_squircle(r, 12.0, TEXT, 34);
        }
        let (iw, ih, m) = assets::icon(kind.taskbar_icons().1);
        p.draw_mask(r.x + (r.w - iw) / 2, r.y + 12, iw, ih, m, ACCENT, 255);
        let label = match kind {
            AppKind::About => "About",
            k => k.name(),
        };
        p.text(&font::SMALL, r.x + (r.w - font::SMALL.width(label)) / 2, r.y + 54, label, TEXT, 255);
    }
    p.fill_rect(Rect::new(20, START_H - 62, START_W - 40, 1), TEXT, 36);
    let (lw, lh, m) = assets::logo_small();
    p.draw_mask(26, START_H - 42, lw, lh, m, TEXT, 255);
    p.text_shadowed(&font::UI_BOLD, 50, START_H - 41, "KonjacOS", TEXT, 255);
    p.text(&font::SMALL, 50 + font::UI_BOLD.width("KonjacOS") + 8, START_H - 39, "0.1.0", TEXT_DIM, 255);
    let pr = Desktop::power_rect();
    if hover == Some(5) {
        p.fill_squircle(pr, 10.0, TEXT, 34);
    }
    let (iw, ih, m) = assets::icon(icon::POWER_20);
    p.draw_mask(pr.x + (pr.w - iw) / 2, pr.y + (pr.h - ih) / 2, iw, ih, m, TEXT, 255);
}

fn paint_menu(p: &mut Painter, m: &Menu) {
    let w = m.rect.w;
    let mut y = MENU_PAD;
    for (i, item) in m.items.iter().enumerate() {
        if item.label.is_empty() {
            p.fill_rect(Rect::new(14, y + MENU_SEP_H / 2, w - 28, 1), TEXT, 40);
            y += MENU_SEP_H;
            continue;
        }
        if m.hover == Some(i) {
            p.fill_squircle(Rect::new(MENU_PAD, y, w - 2 * MENU_PAD, MENU_ITEM_H), 8.0, TEXT, 36);
        }
        p.text_shadowed(&font::UI, 20, y + (MENU_ITEM_H - font::UI.line_height()) / 2, item.label, TEXT, 255);
        y += MENU_ITEM_H;
    }
}

fn paint_cursor(bb: &mut Surface, clip: Rect, mx: i32, my: i32) {
    let x0 = mx - cursor::HOTSPOT_X;
    let y0 = my - cursor::HOTSPOT_Y;
    let r = Rect::new(x0, y0, cursor::WIDTH as i32, cursor::HEIGHT as i32).intersect(&clip);
    for y in r.y..r.bottom() {
        for x in r.x..r.right() {
            if let Some((cr, cg, cb, a)) = cursor::pixel((x - x0) as u32, (y - y0) as u32) {
                if a != 0 {
                    let i = (y * bb.w + x) as usize;
                    bb.px[i] = blend(bb.px[i], rgb(cr, cg, cb), a as u32);
                }
            }
        }
    }
}

/// The desktop task's entry point.
pub fn run() {
    let Some(canvas) = super::take_canvas() else { return };
    let mut d = Desktop::new(canvas);
    d.open_app(AppKind::Terminal);
    d.boot();

    let mut last = timer::ticks();
    loop {
        let now = timer::ticks();
        let steps = now.saturating_sub(last).min(8);
        last = now;
        d.now = now;
        d.poll_input();
        d.poll_sources();
        for _ in 0..steps {
            d.animate();
        }
        d.flush();
        task::sleep_ticks(1);
    }
}
