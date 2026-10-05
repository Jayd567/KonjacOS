//! The desktop: a compositor, window manager and shell UI running as one
//! kernel task.
//!
//! ## Layout
//!
//! - A fully transparent **top bar**: the "K" system menu, the active
//!   app's name, and Window/Help menus. Its dropdowns are separate panes
//!   of glass floating over whatever is underneath.
//! - **Desktop icons** on the wallpaper (see `icons.rs`): every app plus
//!   the root of the disk. Drag them around, rubber-band select them, drop
//!   an app on the taskbar to pin it.
//! - A floating glass **taskbar** at the bottom: Start plus the pinned
//!   and running apps centred (regular icons, filled when hovered or
//!   active, a pill under running ones), the tray (CPU, memory, clock) on
//!   the right.
//! - Glass **windows** in between, resizable from any edge or corner.
//!   Maximizing fills exactly the work area between the two bars, never
//!   sliding under the taskbar.
//! - **Right-click menus** almost everywhere: the desktop, icons, the
//!   taskbar, title bars, and whatever apps offer for their own content.
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
//! is behind it. The pointer's shape follows what's under it (see
//! [`Desktop::pick_cursor`]): resize arrows on window edges, an I-beam
//! over text, a pen over Sketch's paper, and so on.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::Ordering;

use super::apps::{open_command, Action, App, AppKind, ContextItem, MouseEvent, Reply, ACCENT, TEXT, TEXT_DIM};
use super::assets;
use super::font;
use super::glass::{self, Glass, GlassStyle, Scratch};
use super::icon_ids as icon;
use super::icons::{self, Icon, Target};
use super::math::{smoothstep, Spring};
use super::surface::{blend, rgb, Painter, Rect, Surface};
use super::sysmon;
use crate::cursor::{self, Shape as Cursor};
use crate::framebuffer::Canvas;
use crate::{doom_driver, keyboard, mouse, task, timer};

const TOPBAR_H: i32 = 30;
const TASKBAR_H: i32 = 60;
const TASKBAR_MARGIN: i32 = 8;
const TITLE_H: i32 = 40;
const CELL: i32 = 52;
const CELL_GAP: i32 = 4;
/// Most taskbar slots there can be (Start + every app).
const MAX_ITEMS: usize = 1 + AppKind::ALL.len();

/// Damage z-levels, bottom to top (windows take `L_WIN + z index`).
const L_DESK: u16 = 1;
const L_WIN: u16 = 10;
const L_TOPBAR: u16 = 150;
const L_TASKBAR: u16 = 200;
const L_START: u16 = 300;
const L_MENU: u16 = 310;
const L_TOOLTIP: u16 = 400;
/// Things drawn over everything but the pointer: dragged icons and the
/// rubber band.
const L_DRAG: u16 = 900;
const L_CURSOR: u16 = 1000;
/// Damage that must be recomposed but changed nothing visible (the extra
/// backdrop a recomputing panel samples), so it invalidates nothing.
const L_RENDER_ONLY: u16 = u16::MAX;

/// Ticks between two clicks for them to count as a double-click.
const DOUBLE_CLICK_TICKS: u64 = 40;
/// Hover time before a taskbar tooltip appears.
const TOOLTIP_DELAY: u64 = 45;
/// How long the "app starting" pointer shows after launching something.
const LAUNCH_TICKS: u64 = 60;
/// How far the pointer must move with the button down before a press on
/// an icon becomes a drag.
const DRAG_THRESHOLD: i32 = 4;

/// Resize grips: how far outside and inside a window's edge they reach,
/// and how far along an edge a corner grip extends.
const GRIP_OUT: i32 = 6;
const GRIP_IN: i32 = 5;
const GRIP_CORNER: i32 = 22;
const EDGE_L: u8 = 1;
const EDGE_R: u8 = 2;
const EDGE_T: u8 = 4;
const EDGE_B: u8 = 8;

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

    fn resizable(&self) -> bool {
        self.restore.is_none() && self.app.resizable()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuId {
    System,
    Window,
    Help,
    Power,
    Context,
}

#[derive(Clone, Copy)]
enum Cmd {
    Open(AppKind),
    Shell(&'static str),
    /// These act on the menu's target app's window, else the active one.
    Minimize,
    ToggleMaximize,
    Close,
    Pin(AppKind),
    Unpin(AppKind),
    /// One of the target app's own context-menu entries.
    App(u32),
    OpenIcon(usize),
    ShowInFiles(usize),
    ArrangeIcons,
    RefreshIcons,
    ShowDesktop,
}

struct MenuItem {
    /// Empty label = separator.
    label: &'static str,
    /// `None` on a labelled item = greyed out.
    cmd: Option<Cmd>,
}

const SEPARATOR: MenuItem = MenuItem { label: "", cmd: None };
const MENU_ITEM_H: i32 = 30;
const MENU_SEP_H: i32 = 11;
const MENU_PAD: i32 = 6;

fn item(label: &'static str, cmd: Cmd) -> MenuItem {
    MenuItem { label, cmd: Some(cmd) }
}

fn item_if(label: &'static str, cmd: Cmd, enabled: bool) -> MenuItem {
    MenuItem { label, cmd: enabled.then_some(cmd) }
}

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
    /// The app whose window the window commands act on.
    target: Option<AppKind>,
}

impl Menu {
    fn drawn(&self) -> Rect {
        let s = 0.9 + 0.1 * self.spring.value;
        let (w, h) = ((self.rect.w as f32 * s) as i32, (self.rect.h as f32 * s) as i32);
        let y = if self.upward { self.rect.bottom() - h } else { self.rect.y };
        Rect::new(self.rect.x, y, w, h)
    }

    /// The labelled row under `(x, y)`, enabled or not.
    fn row_at(&self, x: i32, y: i32) -> Option<usize> {
        if !self.rect.contains(x, y) {
            return None;
        }
        let mut top = self.rect.y + MENU_PAD;
        for (i, item) in self.items.iter().enumerate() {
            let h = if item.label.is_empty() { MENU_SEP_H } else { MENU_ITEM_H };
            if y >= top && y < top + h {
                return (!item.label.is_empty()).then_some(i);
            }
            top += h;
        }
        None
    }

    /// The enabled item under `(x, y)`.
    fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        self.row_at(x, y).filter(|&i| self.items[i].cmd.is_some())
    }
}

fn menu_size(items: &[MenuItem]) -> (i32, i32) {
    let width = items.iter().map(|it| font::UI.width(it.label)).max().unwrap_or(0) + 56;
    let height = 2 * MENU_PAD + items.iter().map(|it| if it.label.is_empty() { MENU_SEP_H } else { MENU_ITEM_H }).sum::<i32>();
    (width, height)
}

struct StartMenu {
    open: bool,
    spring: Spring,
    glass: Glass,
    /// One of the `START_*` hit codes, or a tile index.
    hover: Option<usize>,
}

const START_W: i32 = 540;
const START_H: i32 = 236;
const TILE_W: i32 = 76;
const TILE_H: i32 = 80;
const TILE_GAP: i32 = 6;
/// Start menu hit codes after the app tiles (`0..AppKind::ALL.len()`).
const START_POWER: usize = 100;
const START_USER: usize = 101;

struct Tooltip {
    text: String,
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

/// What the held left button is doing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grab {
    None,
    /// Moving window `win` by its title bar, `(ox, oy)` from its corner.
    Move { win: usize, ox: i32, oy: i32 },
    /// Resizing window `win` by `edges`, from `start` with the pointer
    /// at `(mx, my)`.
    Resize { win: usize, edges: u8, start: Rect, mx: i32, my: i32 },
    /// Pressed inside an app's client area: it gets `Drag`s and an `Up`.
    Client { kind: AppKind },
    /// Pressed on an icon; becomes `IconDrag` once the pointer moves.
    IconPress { x: i32, y: i32 },
    /// Dragging the selected icons, which started at `(x, y)`.
    IconDrag { x: i32, y: i32 },
    /// Rubber-band selection from `(x, y)`.
    Band { x: i32, y: i32 },
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
    pinned: Vec<AppKind>,
    hover_item: Option<usize>,
    /// What a tooltip would be about: the hovered taskbar item or tray
    /// entry, and since when.
    tip: Option<(Rect, String)>,
    hover_since: u64,
    /// Set by a click on a taskbar item: no tooltip again until the
    /// pointer leaves it.
    tooltip_suppressed: bool,
    hover_alpha: [f32; MAX_ITEMS],
    tray_hover: Option<usize>,
    topbar_hover: Option<MenuId>,
    icons: Vec<Icon>,
    icon_hover: Option<usize>,
    damage: Vec<(Rect, u16)>,
    mx: i32,
    my: i32,
    buttons: u8,
    grab: Grab,
    last_click: (u64, i32, i32),
    cursor: Cursor,
    cursor_frame: u16,
    launching_until: u64,
    sys_seq: u64,
    clock: String,
    date: String,
    long_date: String,
    cpu: String,
    ram: String,
    ram_detail: String,
    now: u64,
}

impl Desktop {
    fn new(canvas: Canvas) -> Self {
        let (w, h) = (canvas.width() as i32, canvas.height() as i32);
        let (mx, my) = mouse::position();
        let mut d = Desktop {
            canvas,
            bb: Surface::new(w, h),
            wall: assets::wallpaper(w, h),
            scratch: Scratch::new(((w / 2 + 64) * (h / 2 + 64)) as usize),
            windows: Vec::new(),
            taskbar: Glass::default(),
            start: StartMenu { open: false, spring: Spring::new(0.0), glass: Glass::default(), hover: None },
            menu: None,
            tooltip: None,
            pinned: AppKind::PINNED.to_vec(),
            hover_item: None,
            tip: None,
            hover_since: 0,
            tooltip_suppressed: false,
            hover_alpha: [0.0; MAX_ITEMS],
            tray_hover: None,
            topbar_hover: None,
            icons: icons::load(),
            icon_hover: None,
            damage: Vec::new(),
            mx,
            my,
            buttons: 0,
            grab: Grab::None,
            last_click: (0, 0, 0),
            cursor: Cursor::Arrow,
            cursor_frame: 0,
            launching_until: 0,
            sys_seq: u64::MAX,
            clock: String::new(),
            date: String::new(),
            long_date: String::new(),
            cpu: String::new(),
            ram: String::new(),
            ram_detail: String::new(),
            now: timer::ticks(),
        };
        let wa = d.work_area();
        icons::arrange(&mut d.icons, wa);
        d
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

    /// The apps on the taskbar after Start: the pinned ones, then any
    /// others that are running.
    fn task_apps(&self) -> Vec<AppKind> {
        let mut apps = self.pinned.clone();
        for w in &self.windows {
            if !apps.contains(&w.kind) {
                apps.push(w.kind);
            }
        }
        apps
    }

    fn item_count(&self) -> usize {
        (1 + self.task_apps().len()).min(MAX_ITEMS)
    }

    fn item_rect(&self, i: usize) -> Rect {
        let tb = self.taskbar_rect();
        let n = self.item_count() as i32;
        let total = n * CELL + (n - 1) * CELL_GAP;
        let x0 = self.screen().w / 2 - total / 2;
        Rect::new(x0 + i as i32 * (CELL + CELL_GAP), tb.y + (TASKBAR_H - CELL) / 2, CELL, CELL)
    }

    fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        (0..self.item_count()).find(|&i| self.item_rect(i).contains(x, y))
    }

    fn tray_rect(&self) -> Rect {
        let tb = self.taskbar_rect();
        Rect::new(tb.right() - 300, tb.y, 300, tb.h)
    }

    /// The tray's hover zones: memory, CPU, clock. Mirrors `paint_tray`.
    fn tray_zones(&self) -> [Rect; 3] {
        let tb = self.taskbar_rect();
        let right = tb.right() - 20;
        let cw = font::UI.width(&self.clock).max(font::SMALL.width(&self.date));
        let clock = Rect::new(right - cw - 10, tb.y + 8, cw + 20, tb.h - 16);
        let mut zones = [Rect::default(), Rect::default(), clock];
        let mut x = right - cw - 26;
        for (z, text) in [&self.ram, &self.cpu].into_iter().enumerate() {
            let tw = font::UI.width(text);
            let end = x;
            x -= tw + 20 + 6;
            zones[z] = Rect::new(x - 8, tb.y + 10, end - x + 16, tb.h - 20);
            x -= 18;
        }
        zones
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
        let n = AppKind::ALL.len() as i32;
        let x0 = (START_W - n * TILE_W - (n - 1) * TILE_GAP) / 2;
        Rect::new(x0 + i as i32 * (TILE_W + TILE_GAP), 48, TILE_W, TILE_H)
    }

    fn power_rect() -> Rect {
        Rect::new(START_W - 58, START_H - 50, 42, 38)
    }

    fn user_rect() -> Rect {
        Rect::new(14, START_H - 52, 220, 42)
    }

    fn start_hit(&self, x: i32, y: i32) -> Option<usize> {
        let r = self.start_rect();
        let (lx, ly) = (x - r.x, y - r.y);
        if Self::power_rect().contains(lx, ly) {
            return Some(START_POWER);
        }
        if Self::user_rect().contains(lx, ly) {
            return Some(START_USER);
        }
        (0..AppKind::ALL.len()).find(|&i| Self::tile_rect(i).contains(lx, ly))
    }

    fn active(&self) -> Option<usize> {
        self.windows.iter().rposition(|w| !w.minimized)
    }

    fn active_kind(&self) -> Option<AppKind> {
        self.active().map(|i| self.windows[i].kind)
    }

    fn window_of(&self, kind: AppKind) -> Option<usize> {
        self.windows.iter().position(|w| w.kind == kind)
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

    /// The resize grip under `(x, y)`: which window, and which of its
    /// edges (`EDGE_*` bits; two for a corner). A window in front blocks
    /// the grips of those behind it.
    fn edge_at(&self, x: i32, y: i32) -> Option<(usize, u8)> {
        for i in (0..self.windows.len()).rev() {
            let w = &self.windows[i];
            if w.minimized {
                continue;
            }
            let d = w.drawn();
            if w.resizable() && d.expand(GRIP_OUT).contains(x, y) {
                let mut e = 0;
                if x < d.x + GRIP_IN {
                    e |= EDGE_L;
                } else if x >= d.right() - GRIP_IN {
                    e |= EDGE_R;
                }
                if y < d.y + GRIP_IN {
                    e |= EDGE_T;
                } else if y >= d.bottom() - GRIP_IN {
                    e |= EDGE_B;
                }
                if e & (EDGE_L | EDGE_R) != 0 && e & (EDGE_T | EDGE_B) == 0 {
                    if y < d.y + GRIP_CORNER {
                        e |= EDGE_T;
                    } else if y >= d.bottom() - GRIP_CORNER {
                        e |= EDGE_B;
                    }
                } else if e & (EDGE_T | EDGE_B) != 0 && e & (EDGE_L | EDGE_R) == 0 {
                    if x < d.x + GRIP_CORNER {
                        e |= EDGE_L;
                    } else if x >= d.right() - GRIP_CORNER {
                        e |= EDGE_R;
                    }
                }
                if e != 0 {
                    return Some((i, e));
                }
            }
            if d.contains(x, y) && w.glass.shape.hit(x - d.x, y - d.y) {
                return None;
            }
        }
        None
    }

    fn icon_area(&self) -> Rect {
        self.work_area()
    }

    fn icon_cell(&self, i: usize) -> Rect {
        let ic = &self.icons[i];
        icons::cell_rect(self.icon_area(), ic.col, ic.row)
    }

    /// The icon under `(x, y)`, if the desktop itself is what's there.
    fn icon_at(&self, x: i32, y: i32) -> Option<usize> {
        (0..self.icons.len()).find(|&i| icons::hit_rect(self.icon_cell(i)).contains(x, y))
    }

    /// Whether `(x, y)` is bare desktop: no bar, window, menu or Start.
    fn on_desktop(&self, x: i32, y: i32) -> bool {
        y >= TOPBAR_H
            && !self.taskbar_rect().contains(x, y)
            && self.window_at(x, y).is_none()
            && self.edge_at(x, y).is_none()
            && !self.menu.as_ref().is_some_and(|m| m.rect.contains(x, y))
            && !(self.start.open && self.start_rect().contains(x, y))
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
        let img = cursor::image(self.cursor);
        let r = Rect::new(self.mx - img.hot_x, self.my - img.hot_y, img.w, img.h);
        self.damage(r, L_CURSOR);
    }

    fn damage_taskbar(&mut self) {
        let r = self.taskbar_rect();
        self.damage(r, L_TASKBAR);
    }

    fn damage_icon(&mut self, i: usize) {
        let r = self.icon_cell(i);
        self.damage(r, L_DESK);
    }

    /// Where the dragged icons are drawn right now, all together.
    fn drag_bounds(&self) -> Rect {
        let Grab::IconDrag { x, y } = self.grab else { return Rect::default() };
        let (dx, dy) = (self.mx - x, self.my - y);
        let mut r = Rect::default();
        for i in 0..self.icons.len() {
            if self.icons[i].selected {
                let c = self.icon_cell(i).offset(dx, dy);
                r = if r.is_empty() { c } else { r.union(&c) };
            }
        }
        r
    }

    fn band_rect(&self) -> Rect {
        let Grab::Band { x, y } = self.grab else { return Rect::default() };
        Rect::new(x.min(self.mx), y.min(self.my), (x - self.mx).abs() + 1, (y - self.my).abs() + 1)
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
        let apps = self.task_apps();
        let items: Vec<Rect> = (0..self.item_count()).map(|i| self.item_rect(i)).collect();
        let active_kind = self.active_kind();
        let start_drawn = self.start_drawn();
        let start_open = self.start_visible();
        let icon_area = self.icon_area();
        let drag = match self.grab {
            Grab::IconDrag { x, y } => Some((self.mx - x, self.my - y)),
            _ => None,
        };
        let band = self.band_rect();
        let tray_hover = self.tray_hover.map(|z| self.tray_zones()[z]);
        let cursor_img = cursor::image(self.cursor);

        // Occlusion: if a window's opaque content (DOOM's frame, Sketch's
        // paper) covers this whole region, nothing beneath it can show --
        // start there.
        let first = self.windows.iter().enumerate().rev().find_map(|(i, w)| {
            if w.minimized || w.opacity() < 255 {
                return None;
            }
            let d = w.drawn();
            let (cw, ch) = w.client_size();
            let r = w.app.opaque_rect(cw, ch)?.offset(d.x, d.y + TITLE_H);
            (r.intersect(&clip) == clip).then_some(i)
        });

        let Desktop { bb, wall, scratch, windows, taskbar, start, menu, tooltip, hover_alpha, topbar_hover, icons: desk_icons, icon_hover, mx, my, clock, date, cpu, ram, cursor_frame, .. } = self;
        let (sw, sh) = (bb.w, bb.h);

        if first.is_none() {
            bb.painter(clip).blit(0, 0, sw, sh, wall, sw);
            let mut p = bb.painter(clip);
            for (i, ic) in desk_icons.iter().enumerate() {
                let cell = icons::cell_rect(icon_area, ic.col, ic.row);
                if cell.intersects(&clip) {
                    let alpha = if drag.is_some() && ic.selected { 90 } else { 255 };
                    icons::paint(&mut p, cell, ic, *icon_hover == Some(i), alpha);
                }
            }
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

        paint_topbar(&mut bb.painter(clip), &topbar, active_name, *topbar_hover, menu.as_ref().filter(|m| m.id != MenuId::Context).map(|m| m.id));

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
                let kind = apps[i - 1];
                let is_active = active_kind == Some(kind);
                let (regular, filled) = kind.taskbar_icons();
                let id = if is_active || h > 0.5 { filled } else { regular };
                let (iw, ih, m) = assets::icon(id);
                p.draw_mask(r.x + (r.w - iw) / 2, r.y + (r.h - ih) / 2 - 2, iw, ih, m, TEXT, 255);
                if windows.iter().any(|w| w.kind == kind) {
                    let pw = if is_active { 18 } else { 6 };
                    p.fill_squircle(Rect::new(r.x + (r.w - pw) / 2, r.bottom() - 5, pw, 3), 1.5, TEXT, if is_active { 235 } else { 150 });
                }
            }
            if let Some(z) = tray_hover {
                p.fill_squircle(z, 10.0, TEXT, 26);
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
            let tx = t.rect.x + (t.rect.w - font::UI.width(&t.text)) / 2;
            p.text_shadowed(&font::UI, tx, t.rect.y + 7, &t.text, TEXT, 255);
        }

        if let Some((dx, dy)) = drag {
            let mut p = bb.painter(clip);
            for ic in desk_icons.iter().filter(|ic| ic.selected) {
                let cell = icons::cell_rect(icon_area, ic.col, ic.row).offset(dx, dy);
                if cell.intersects(&clip) {
                    icons::paint(&mut p, cell, ic, false, 210);
                }
            }
        }
        if !band.is_empty() {
            let mut p = bb.painter(clip);
            p.fill_rect(band, ACCENT, 40);
            for edge in [
                Rect::new(band.x, band.y, band.w, 1),
                Rect::new(band.x, band.bottom() - 1, band.w, 1),
                Rect::new(band.x, band.y, 1, band.h),
                Rect::new(band.right() - 1, band.y, 1, band.h),
            ] {
                p.fill_rect(edge, ACCENT, 200);
            }
        }

        paint_cursor(bb, clip, *mx, *my, &cursor_img, *cursor_frame);
    }

    // --- Windows ----------------------------------------------------------

    fn open_app(&mut self, kind: AppKind) {
        if let Some(i) = self.window_of(kind) {
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
        self.launching_until = self.now + LAUNCH_TICKS;
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
        // The set of taskbar items may have changed, shifting them all.
        self.damage_taskbar();
    }

    /// Bookkeeping after window `i` left `windows`: a grab on it ends, a
    /// grab on a window above it follows the index shift.
    fn window_removed(&mut self, i: usize) {
        match &mut self.grab {
            Grab::Move { win, .. } | Grab::Resize { win, .. } if *win == i => self.grab = Grab::None,
            Grab::Move { win, .. } | Grab::Resize { win, .. } if *win > i => *win -= 1,
            _ => {}
        }
        self.focus_changed();
    }

    fn close_window(&mut self, i: usize) {
        self.damage_window(i);
        let mut w = self.windows.remove(i);
        w.app.closed();
        self.window_removed(i);
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

    /// Resizes window `i` from `start` by dragging `edges` by `(dx, dy)`,
    /// keeping it at least the app's minimum size and inside the screen
    /// (and below the top bar, above the taskbar).
    fn resize_window(&mut self, i: usize, edges: u8, start: Rect, dx: i32, dy: i32) {
        let wa = self.work_area();
        let s = self.screen();
        let (mw, mh) = self.windows[i].app.min_size();
        let (min_w, min_h) = (mw.max(300), mh + TITLE_H);
        let mut r = start;
        if edges & EDGE_L != 0 {
            let x = (start.x + dx).min(start.right() - min_w).max(0);
            r.x = x;
            r.w = start.right() - x;
        }
        if edges & EDGE_R != 0 {
            r.w = (start.w + dx).max(min_w).min(s.w - r.x);
        }
        if edges & EDGE_T != 0 {
            let y = (start.y + dy).min(start.bottom() - min_h).max(wa.y);
            r.y = y;
            r.h = start.bottom() - y;
        }
        if edges & EDGE_B != 0 {
            r.h = (start.h + dy).max(min_h).min(wa.bottom() - r.y);
        }
        if r == self.windows[i].rect {
            return;
        }
        self.damage_window(i);
        let w = &mut self.windows[i];
        w.rect = r;
        let (cw, ch) = w.client_size();
        w.app.resized(cw, ch);
        self.damage_window(i);
    }

    /// Taskbar click: launch, restore, focus or minimize.
    fn taskbar_click(&mut self, kind: AppKind) {
        if kind == AppKind::Doom && self.window_of(kind).is_none() {
            match crate::commands::launch_doom() {
                Ok(_) => self.open_app(kind),
                Err(e) => {
                    crate::println!("doom: {e}");
                    self.open_app(AppKind::Terminal);
                }
            }
            return;
        }
        match self.window_of(kind) {
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

    /// Start menu tiles, desktop icons and menus launch (or bring
    /// forward) an app, never minimize it.
    fn launch(&mut self, kind: AppKind) {
        if kind == AppKind::Doom {
            match self.window_of(kind) {
                Some(i) => {
                    self.windows[i].minimized = false;
                    self.focus(i);
                }
                None => self.taskbar_click(kind),
            }
        } else {
            self.open_app(kind);
        }
    }

    fn shell_command(&mut self, cmd: &str) {
        keyboard::inject(cmd);
        self.open_app(AppKind::Terminal);
    }

    /// Brings Files forward showing folder `dir`, with `select` picked.
    fn open_files_at(&mut self, dir: &str, select: Option<&str>) {
        self.open_app(AppKind::Files);
        if let Some(i) = self.window_of(AppKind::Files) {
            self.windows[i].app.navigate(dir, select);
            self.damage_client(i);
        }
    }

    fn apply_reply(&mut self, i: usize, reply: Reply) {
        if reply.repaint {
            self.damage_client(i);
        } else if let Some(r) = reply.damage {
            let d = self.windows[i].drawn();
            let client = Rect::new(d.x, d.y + TITLE_H, d.w, d.h - TITLE_H);
            self.damage(r.offset(client.x, client.y).intersect(&client), L_WIN + i as u16);
        }
        if let Some(Action::Shell(cmd)) = reply.action {
            self.shell_command(&cmd);
        }
    }

    fn set_pinned(&mut self, kind: AppKind, pinned: bool) {
        if pinned && !self.pinned.contains(&kind) {
            self.pinned.push(kind);
        } else if !pinned {
            self.pinned.retain(|&k| k != kind);
        }
        self.hide_tooltip();
        self.damage_taskbar();
    }

    // --- Desktop icons ------------------------------------------------------

    fn open_icon(&mut self, i: usize) {
        match self.icons[i].target.clone() {
            Target::App(kind) => self.launch(kind),
            Target::Dir(path) => self.open_files_at(&path, None),
            Target::File(path) => {
                if path.to_ascii_lowercase().ends_with(".wad") {
                    self.launch(AppKind::Doom);
                } else if let Some(cmd) = open_command(&path) {
                    self.shell_command(&cmd);
                }
            }
        }
    }

    fn show_icon_in_files(&mut self, i: usize) {
        if let Target::File(path) | Target::Dir(path) = self.icons[i].target.clone() {
            let cut = path.rfind('/').unwrap_or(0);
            let dir = if cut == 0 { "/" } else { &path[..cut] };
            self.open_files_at(dir, Some(&path[cut + 1..]));
        }
    }

    fn select_icons(&mut self, pick: impl Fn(usize, &Icon) -> bool) {
        for i in 0..self.icons.len() {
            let sel = pick(i, &self.icons[i]);
            if sel != self.icons[i].selected {
                self.icons[i].selected = sel;
                self.damage_icon(i);
            }
        }
    }

    /// Lays the icons out from scratch, in order.
    fn arrange_icons(&mut self) {
        for i in 0..self.icons.len() {
            self.damage_icon(i);
        }
        let area = self.icon_area();
        icons::arrange(&mut self.icons, area);
        for i in 0..self.icons.len() {
            self.damage_icon(i);
        }
    }

    /// Re-reads the disk's root folder; icons that are still there keep
    /// their place, new ones go in the first free cells.
    fn refresh_icons(&mut self) {
        for i in 0..self.icons.len() {
            self.damage_icon(i);
        }
        let area = self.icon_area();
        let mut fresh = icons::load();
        let mut taken = Vec::new();
        let mut placed = alloc::vec![false; fresh.len()];
        for (n, ic) in fresh.iter_mut().enumerate() {
            if let Some(old) = self.icons.iter().find(|o| o.target == ic.target) {
                ic.col = old.col;
                ic.row = old.row;
                taken.push((ic.col, ic.row));
                placed[n] = true;
            }
        }
        for (n, ic) in fresh.iter_mut().enumerate() {
            if !placed[n] {
                let (c, r) = icons::nearest_free(area, 0, 0, &taken);
                ic.col = c;
                ic.row = r;
                taken.push((c, r));
            }
        }
        self.icons = fresh;
        self.icon_hover = None;
        for i in 0..self.icons.len() {
            self.damage_icon(i);
        }
    }

    /// Where a drop of the selected icons, dragged by `(dx, dy)`, would
    /// pin something: any selected app that isn't on the taskbar yet.
    fn droppable_apps(&self) -> Vec<AppKind> {
        self.icons.iter().filter(|ic| ic.selected).filter_map(|ic| ic.app()).filter(|k| !self.pinned.contains(k)).collect()
    }

    fn drop_icons(&mut self, dx: i32, dy: i32) {
        let (x, y) = (self.mx, self.my);
        if self.taskbar_rect().contains(x, y) {
            for kind in self.droppable_apps() {
                self.set_pinned(kind, true);
            }
            return;
        }
        if !self.on_desktop(x, y) {
            return;
        }
        let area = self.icon_area();
        let mut taken: Vec<(i32, i32)> = self.icons.iter().filter(|ic| !ic.selected).map(|ic| (ic.col, ic.row)).collect();
        for i in 0..self.icons.len() {
            if !self.icons[i].selected {
                continue;
            }
            self.damage_icon(i);
            let c = self.icon_cell(i).offset(dx, dy);
            let (col, row) = icons::cell_at(area, c.x + c.w / 2, c.y + c.h / 2);
            let (col, row) = if taken.contains(&(col, row)) { icons::nearest_free(area, col, row, &taken) } else { (col, row) };
            self.icons[i].col = col;
            self.icons[i].row = row;
            taken.push((col, row));
            self.damage_icon(i);
        }
    }

    // --- Commands -----------------------------------------------------------

    fn run_cmd(&mut self, cmd: Cmd, target: Option<AppKind>) {
        // A menu about a particular app acts on that app's window only.
        let win = match target {
            Some(k) => self.window_of(k),
            None => self.active(),
        };
        match cmd {
            Cmd::Open(kind) => self.launch(kind),
            Cmd::Shell(s) => self.shell_command(s),
            Cmd::Minimize => {
                if let Some(i) = win {
                    self.minimize(i);
                }
            }
            Cmd::ToggleMaximize => {
                if let Some(i) = win {
                    self.toggle_maximize(i);
                }
            }
            Cmd::Close => {
                if let Some(i) = win {
                    self.close_window(i);
                }
            }
            Cmd::Pin(kind) => self.set_pinned(kind, true),
            Cmd::Unpin(kind) => self.set_pinned(kind, false),
            Cmd::App(id) => {
                if let Some(i) = target.and_then(|k| self.window_of(k)) {
                    let reply = self.windows[i].app.context_cmd(id);
                    self.apply_reply(i, reply);
                }
            }
            Cmd::OpenIcon(i) => self.open_icon(i),
            Cmd::ShowInFiles(i) => self.show_icon_in_files(i),
            Cmd::ArrangeIcons => self.arrange_icons(),
            Cmd::RefreshIcons => self.refresh_icons(),
            Cmd::ShowDesktop => {
                for i in 0..self.windows.len() {
                    if !self.windows[i].minimized {
                        self.minimize(i);
                    }
                }
            }
        }
    }

    // --- Menus ------------------------------------------------------------

    fn open_menu(&mut self, id: MenuId) {
        let maximized = self.active().is_some_and(|i| self.windows[i].restore.is_some());
        let items = match id {
            MenuId::System => alloc::vec![
                item("About KonjacOS", Cmd::Open(AppKind::About)),
                item("System Monitor", Cmd::Open(AppKind::Monitor)),
                SEPARATOR,
                item("Restart...", Cmd::Shell("reboot\n")),
                item("Shut Down...", Cmd::Shell("halt\n")),
            ],
            MenuId::Window => {
                let any = self.active().is_some();
                alloc::vec![
                    item_if("Minimize", Cmd::Minimize, any),
                    item_if(if maximized { "Restore" } else { "Maximize" }, Cmd::ToggleMaximize, any),
                    SEPARATOR,
                    item_if("Close", Cmd::Close, any),
                ]
            }
            MenuId::Help => alloc::vec![
                item("Shell Commands", Cmd::Shell("help\n")),
                item("About KonjacOS", Cmd::Open(AppKind::About)),
            ],
            MenuId::Power => alloc::vec![item("Restart", Cmd::Shell("reboot\n")), item("Shut Down", Cmd::Shell("halt\n"))],
            MenuId::Context => return,
        };
        let (width, height) = menu_size(&items);
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
        self.show_menu(id, items, rect, upward, None);
    }

    fn show_menu(&mut self, id: MenuId, items: Vec<MenuItem>, rect: Rect, upward: bool, target: Option<AppKind>) {
        self.close_menu();
        self.hide_tooltip();
        let mut spring = Spring::new(0.0);
        spring.target = 1.0;
        self.menu = Some(Menu { id, rect, upward, items, hover: None, spring, glass: Glass::default(), target });
        self.damage(rect.expand(glass::MENU.reach()), L_MENU);
        self.damage(Rect::new(0, 0, self.screen().w, TOPBAR_H), L_TOPBAR);
    }

    /// Opens a right-click menu at the pointer, flipped to stay on screen.
    fn context_menu(&mut self, items: Vec<MenuItem>, target: Option<AppKind>) {
        if items.is_empty() {
            return;
        }
        let (w, h) = menu_size(&items);
        let s = self.screen();
        let x = self.mx.min(s.w - w - 6).max(6);
        let (y, upward) = if self.my + h > s.h - 6 { ((self.my - h).max(6), true) } else { (self.my, false) };
        self.show_menu(MenuId::Context, items, Rect::new(x, y, w, h), upward, target);
    }

    /// Opens a right-click menu just above taskbar item `i`.
    fn taskbar_menu(&mut self, i: usize, items: Vec<MenuItem>, target: Option<AppKind>) {
        let (w, h) = menu_size(&items);
        let r = self.item_rect(i);
        let s = self.screen();
        let x = (r.x + r.w / 2 - w / 2).clamp(6, s.w - w - 6);
        let y = self.taskbar_rect().y - 10 - h;
        self.show_menu(MenuId::Context, items, Rect::new(x, y, w, h), true, target);
    }

    /// The right-click menu for app `kind` on the taskbar, Start or the
    /// desktop: open it, pin or unpin it, close its window.
    fn app_menu(&self, kind: AppKind, open: Cmd, open_label: &'static str) -> Vec<MenuItem> {
        let running = self.window_of(kind).is_some();
        let mut items = alloc::vec![item(open_label, open), SEPARATOR];
        items.push(if self.pinned.contains(&kind) { item("Unpin from Taskbar", Cmd::Unpin(kind)) } else { item("Pin to Taskbar", Cmd::Pin(kind)) });
        if running {
            items.push(item("Close Window", Cmd::Close));
        }
        items
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

    /// What a tooltip at `(x, y)` would say, and about which rectangle.
    fn tip_at(&self, x: i32, y: i32) -> Option<(Rect, String)> {
        if !self.taskbar_rect().contains(x, y) {
            return None;
        }
        if let Some(i) = self.item_at(x, y) {
            let name = if i == 0 { "Start" } else { self.task_apps()[i - 1].name() };
            return Some((self.item_rect(i), String::from(name)));
        }
        let zones = self.tray_zones();
        let z = zones.iter().position(|r| r.contains(x, y))?;
        let text = match z {
            0 => self.ram_detail.clone(),
            1 => {
                let mut s = String::from("CPU usage ");
                s.push_str(&self.cpu);
                s
            }
            _ => self.long_date.clone(),
        };
        Some((zones[z], text))
    }

    // --- Input ------------------------------------------------------------

    fn poll_input(&mut self) {
        let (x, y) = mouse::position();
        let b = mouse::buttons();
        if (x, y) != (self.mx, self.my) {
            self.damage_cursor();
            // Dragged icons and the rubber band follow the pointer: clear
            // where they were.
            let (drag, band) = (self.drag_bounds(), self.band_rect());
            self.damage(drag, L_DRAG);
            self.damage(band.expand(1), L_DRAG);
            self.mx = x;
            self.my = y;
            self.damage_cursor();
            self.on_move();
        }
        let pressed = b & !self.buttons;
        let released = self.buttons & !b;
        self.buttons = b;
        if pressed & mouse::LEFT_BUTTON != 0 {
            self.on_press();
        } else if released & mouse::LEFT_BUTTON != 0 {
            self.on_release();
        }
        if pressed & mouse::RIGHT_BUTTON != 0 && self.grab == Grab::None {
            self.on_right_press();
        }
    }

    fn on_move(&mut self) {
        let (x, y) = (self.mx, self.my);
        match self.grab {
            Grab::Move { win, ox, oy } => {
                if let Some(w) = self.windows.get(win) {
                    if let Some(r) = w.restore {
                        // Dragging a maximized window restores it under the
                        // cursor, like other desktops do.
                        let ratio = ox as f32 / w.rect.w as f32;
                        self.damage_window(win);
                        let w = &mut self.windows[win];
                        w.restore = None;
                        let nox = (r.w as f32 * ratio) as i32;
                        w.rect = Rect::new(x - nox, y - oy, r.w, r.h);
                        let (cw, ch) = w.client_size();
                        w.app.resized(cw, ch);
                        self.grab = Grab::Move { win, ox: nox, oy };
                    } else {
                        self.move_window(win, x - ox, y - oy);
                    }
                }
                return;
            }
            Grab::Resize { win, edges, start, mx, my } => {
                if win < self.windows.len() {
                    self.resize_window(win, edges, start, x - mx, y - my);
                }
                return;
            }
            Grab::Client { kind } => {
                if let Some(i) = self.window_of(kind) {
                    let d = self.windows[i].drawn();
                    let (cw, ch) = self.windows[i].client_size();
                    let reply = self.windows[i].app.mouse(MouseEvent::Drag, x - d.x, y - d.y - TITLE_H, cw, ch);
                    self.apply_reply(i, reply);
                }
                return;
            }
            Grab::IconPress { x: px, y: py } => {
                if (x - px).abs() > DRAG_THRESHOLD || (y - py).abs() > DRAG_THRESHOLD {
                    self.grab = Grab::IconDrag { x: px, y: py };
                    self.icon_hover = None;
                    for i in 0..self.icons.len() {
                        if self.icons[i].selected {
                            self.damage_icon(i);
                        }
                    }
                    let r = self.drag_bounds();
                    self.damage(r, L_DRAG);
                }
                return;
            }
            Grab::IconDrag { .. } => {
                let r = self.drag_bounds();
                self.damage(r, L_DRAG);
                return;
            }
            Grab::Band { .. } => {
                let r = self.band_rect();
                self.damage(r.expand(1), L_DRAG);
                let area = self.icon_area();
                self.select_icons(|_, ic| icons::hit_rect(icons::cell_rect(area, ic.col, ic.row)).intersects(&r));
                return;
            }
            Grab::None => {}
        }

        let item = if self.taskbar_rect().contains(x, y) { self.item_at(x, y) } else { None };
        self.hover_item = item;
        let tip = self.tip_at(x, y);
        if tip.as_ref().map(|t| t.0) != self.tip.as_ref().map(|t| t.0) {
            self.tip = tip;
            self.hover_since = self.now;
            self.tooltip_suppressed = false;
            self.hide_tooltip();
        }
        let tray = if self.taskbar_rect().contains(x, y) { self.tray_zones().iter().position(|r| r.contains(x, y)) } else { None };
        if tray != self.tray_hover {
            self.tray_hover = tray;
            let r = self.tray_rect();
            self.damage(r, L_TASKBAR);
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

        let free = self.menu.is_none() && !self.start.open;
        let icon = if free && self.on_desktop(x, y) { self.icon_at(x, y) } else { None };
        if icon != self.icon_hover {
            if let Some(i) = self.icon_hover {
                self.damage_icon(i);
            }
            self.icon_hover = icon;
            if let Some(i) = icon {
                self.damage_icon(i);
            }
        }

        let over = if free && y >= TOPBAR_H && self.edge_at(x, y).is_none() { self.window_at(x, y) } else { None };
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

    fn on_release(&mut self) {
        let grab = core::mem::replace(&mut self.grab, Grab::None);
        match grab {
            Grab::Client { kind } => {
                if let Some(i) = self.window_of(kind) {
                    let d = self.windows[i].drawn();
                    let (cw, ch) = self.windows[i].client_size();
                    let reply = self.windows[i].app.mouse(MouseEvent::Up, self.mx - d.x, self.my - d.y - TITLE_H, cw, ch);
                    self.apply_reply(i, reply);
                }
            }
            Grab::IconDrag { x, y } => {
                self.grab = grab;
                let r = self.drag_bounds();
                self.grab = Grab::None;
                self.damage(r, L_DRAG);
                self.drop_icons(self.mx - x, self.my - y);
                for i in 0..self.icons.len() {
                    if self.icons[i].selected {
                        self.damage_icon(i);
                    }
                }
            }
            Grab::Band { .. } => {
                self.grab = grab;
                let r = self.band_rect();
                self.grab = Grab::None;
                self.damage(r.expand(1), L_DRAG);
            }
            _ => {}
        }
        // Whatever is under the pointer now gets its hover state back.
        self.on_move();
    }

    fn on_press(&mut self) {
        let (x, y) = (self.mx, self.my);
        let (lt, lx, ly) = self.last_click;
        let double = self.now - lt <= DOUBLE_CLICK_TICKS && (x - lx).abs() < 6 && (y - ly).abs() < 6;
        self.last_click = if double { (0, x, y) } else { (self.now, x, y) };

        if let Some(m) = &self.menu {
            let (id, rect, target, hit) = (m.id, m.rect, m.target, m.item_at(x, y));
            if rect.contains(x, y) {
                if let Some(cmd) = hit.and_then(|i| self.menu.as_ref().unwrap().items[i].cmd) {
                    self.close_menu();
                    if id == MenuId::Power {
                        self.close_start();
                    }
                    self.run_cmd(cmd, target);
                }
                return;
            }
            self.close_menu();
            if id != MenuId::Context {
                if let Some(other) = self.topbar_item_at(x, y).filter(|&o| o != id) {
                    self.open_menu(other);
                }
            }
            if !(self.start.open && self.start_rect().contains(x, y)) {
                return;
            }
        }

        if self.start.open {
            if self.start_rect().contains(x, y) {
                match self.start_hit(x, y) {
                    Some(START_POWER) => self.open_menu(MenuId::Power),
                    Some(START_USER) => {
                        self.close_start();
                        self.launch(AppKind::About);
                    }
                    Some(i) => {
                        self.close_start();
                        self.launch(AppKind::ALL[i]);
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
                Some(i) => self.taskbar_click(self.task_apps()[i - 1]),
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

        if let Some((i, edges)) = self.edge_at(x, y) {
            let i = self.focus(i);
            self.grab = Grab::Resize { win: i, edges, start: self.windows[i].rect, mx: x, my: y };
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
                    None => self.grab = Grab::Move { win: i, ox: lx, oy: ly },
                }
            } else {
                let (cw, ch) = self.windows[i].client_size();
                let ev = if double { MouseEvent::DoubleClick } else { MouseEvent::Down };
                self.grab = Grab::Client { kind: self.windows[i].kind };
                let reply = self.windows[i].app.mouse(ev, lx, ly - TITLE_H, cw, ch);
                self.apply_reply(i, reply);
            }
            return;
        }

        // The desktop itself.
        match self.icon_at(x, y) {
            Some(i) if double => {
                self.select_icons(|j, _| j == i);
                self.open_icon(i);
            }
            Some(i) => {
                if !self.icons[i].selected {
                    self.select_icons(|j, _| j == i);
                }
                self.grab = Grab::IconPress { x, y };
            }
            None => {
                self.select_icons(|_, _| false);
                self.grab = Grab::Band { x, y };
            }
        }
    }

    fn on_right_press(&mut self) {
        let (x, y) = (self.mx, self.my);
        if let Some(m) = &self.menu {
            let inside = m.rect.contains(x, y);
            self.close_menu();
            if inside {
                return;
            }
        }

        if self.start.open {
            if self.start_rect().contains(x, y) {
                if let Some(i) = self.start_hit(x, y).filter(|&i| i < AppKind::ALL.len()) {
                    let kind = AppKind::ALL[i];
                    let items = self.app_menu(kind, Cmd::Open(kind), "Open");
                    self.context_menu(items, Some(kind));
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
                Some(0) => {
                    let items = alloc::vec![
                        item("Terminal", Cmd::Open(AppKind::Terminal)),
                        item("Files", Cmd::Open(AppKind::Files)),
                        item("System Monitor", Cmd::Open(AppKind::Monitor)),
                        SEPARATOR,
                        item("Show Desktop", Cmd::ShowDesktop),
                        SEPARATOR,
                        item("Restart", Cmd::Shell("reboot\n")),
                        item("Shut Down", Cmd::Shell("halt\n")),
                    ];
                    self.taskbar_menu(0, items, None);
                }
                Some(i) => {
                    let kind = self.task_apps()[i - 1];
                    let items = self.app_menu(kind, Cmd::Open(kind), kind.name());
                    self.taskbar_menu(i, items, Some(kind));
                }
                None => {
                    let items = alloc::vec![item("System Monitor", Cmd::Open(AppKind::Monitor)), item("Show Desktop", Cmd::ShowDesktop)];
                    self.context_menu(items, None);
                }
            }
            return;
        }

        if y < TOPBAR_H {
            return;
        }

        if self.edge_at(x, y).is_some() {
            return;
        }

        if let Some(i) = self.window_at(x, y) {
            let i = self.focus(i);
            let w = &self.windows[i];
            let kind = w.kind;
            let d = w.drawn();
            let (lx, ly) = (x - d.x, y - d.y);
            if ly < TITLE_H {
                let maximized = w.restore.is_some();
                let items = alloc::vec![
                    item_if("Restore", Cmd::ToggleMaximize, maximized),
                    item("Minimize", Cmd::Minimize),
                    item_if("Maximize", Cmd::ToggleMaximize, !maximized),
                    SEPARATOR,
                    item("Close", Cmd::Close),
                ];
                self.context_menu(items, Some(kind));
            } else {
                let (cw, ch) = w.client_size();
                let offered: Vec<ContextItem> = self.windows[i].app.context_menu(lx, ly - TITLE_H, cw, ch);
                self.damage_client(i);
                let items = offered.into_iter().map(|(label, id)| MenuItem { label, cmd: id.map(Cmd::App) }).collect();
                self.context_menu(items, Some(kind));
            }
            return;
        }

        // The desktop itself.
        match self.icon_at(x, y) {
            Some(i) => {
                if !self.icons[i].selected {
                    self.select_icons(|j, _| j == i);
                }
                let items = match &self.icons[i].target {
                    Target::App(kind) => {
                        let kind = *kind;
                        let items = self.app_menu(kind, Cmd::OpenIcon(i), "Open");
                        self.context_menu(items, Some(kind));
                        return;
                    }
                    Target::Dir(_) => alloc::vec![item("Open", Cmd::OpenIcon(i))],
                    Target::File(path) => {
                        let can_open = open_command(path).is_some();
                        alloc::vec![item_if("Open", Cmd::OpenIcon(i), can_open), item("Show in Files", Cmd::ShowInFiles(i))]
                    }
                };
                self.context_menu(items, None);
            }
            None => {
                self.select_icons(|_, _| false);
                let items = alloc::vec![
                    item("Open Terminal", Cmd::Open(AppKind::Terminal)),
                    item("Open Files", Cmd::Open(AppKind::Files)),
                    SEPARATOR,
                    item("Arrange Icons", Cmd::ArrangeIcons),
                    item("Refresh", Cmd::RefreshIcons),
                    item("Show Desktop", Cmd::ShowDesktop),
                    SEPARATOR,
                    item("About KonjacOS", Cmd::Open(AppKind::About)),
                ];
                self.context_menu(items, None);
            }
        }
    }

    // --- Pointer ------------------------------------------------------------

    /// The pointer for what's under it right now, or for what the held
    /// button is doing.
    fn pick_cursor(&self) -> Cursor {
        let (x, y) = (self.mx, self.my);
        match self.grab {
            Grab::Move { .. } => return Cursor::Move,
            Grab::Resize { edges, .. } => return edge_cursor(edges),
            Grab::IconDrag { .. } => {
                return if self.taskbar_rect().contains(x, y) {
                    if self.droppable_apps().is_empty() {
                        Cursor::No
                    } else {
                        Cursor::Pin
                    }
                } else if self.on_desktop(x, y) {
                    Cursor::Move
                } else {
                    Cursor::No
                };
            }
            Grab::Band { .. } => return Cursor::Cross,
            Grab::Client { kind } => {
                if let Some(i) = self.window_of(kind) {
                    let w = &self.windows[i];
                    let d = w.drawn();
                    let (cw, ch) = w.client_size();
                    return w.app.cursor(x - d.x, y - d.y - TITLE_H, cw, ch);
                }
            }
            Grab::IconPress { .. } | Grab::None => {}
        }

        let hover = self.hover_cursor(x, y);
        let launching = self.now < self.launching_until || self.windows.iter().any(|w| w.app.loading());
        if hover == Cursor::Arrow && launching {
            Cursor::Working
        } else {
            hover
        }
    }

    fn hover_cursor(&self, x: i32, y: i32) -> Cursor {
        if let Some(m) = &self.menu {
            if m.rect.contains(x, y) {
                return match m.row_at(x, y) {
                    Some(i) if m.items[i].cmd.is_none() => Cursor::No,
                    Some(_) if m.id == MenuId::Help => Cursor::Help,
                    _ => Cursor::Arrow,
                };
            }
        }
        if self.start.open && self.start_rect().contains(x, y) {
            return match self.start_hit(x, y) {
                Some(START_USER) => Cursor::Person,
                Some(_) => Cursor::Hand,
                None => Cursor::Arrow,
            };
        }
        if self.taskbar_rect().contains(x, y) {
            return if self.tray_zones().iter().any(|r| r.contains(x, y)) { Cursor::Help } else { Cursor::Arrow };
        }
        if y < TOPBAR_H {
            return if self.topbar_item_at(x, y) == Some(MenuId::Help) { Cursor::Help } else { Cursor::Arrow };
        }
        if self.menu.is_some() || self.start.open {
            return Cursor::Arrow;
        }
        if let Some((_, edges)) = self.edge_at(x, y) {
            return edge_cursor(edges);
        }
        if let Some(i) = self.window_at(x, y) {
            let w = &self.windows[i];
            let d = w.drawn();
            let (cw, ch) = w.client_size();
            if y - d.y >= TITLE_H {
                return w.app.cursor(x - d.x, y - d.y - TITLE_H, cw, ch);
            }
        }
        Cursor::Arrow
    }

    /// Switches the pointer's shape (or animation frame) when it changes.
    fn update_cursor(&mut self) {
        let shape = self.pick_cursor();
        let frame = cursor::image(shape).frame_at(self.now);
        if (shape, frame) != (self.cursor, self.cursor_frame) {
            self.damage_cursor();
            self.cursor = shape;
            self.cursor_frame = frame;
            self.damage_cursor();
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
            self.long_date.clear();
            let _ = write!(self.long_date, "{}, {} {}, {}", weekday(t.year, t.month, t.day), month_name(t.month), t.day, t.year);
            self.ram_detail.clear();
            let _ = write!(self.ram_detail, "Memory: {} of {} MiB", s.mem_used / (1024 * 1024), s.mem_total / (1024 * 1024));
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
            let window = self.window_of(AppKind::Doom);
            match (running, window) {
                (true, None) => self.open_app(AppKind::Doom),
                (false, Some(i)) => {
                    self.damage_window(i);
                    self.windows.remove(i);
                    self.window_removed(i);
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
        for i in 0..self.item_count() {
            let target = if self.hover_item == Some(i) { 1.0 } else { 0.0 };
            let cur = self.hover_alpha[i];
            if cur != target {
                self.hover_alpha[i] = if target > cur { (cur + 0.125).min(1.0) } else { (cur - 0.125).max(0.0) };
                let r = self.item_rect(i);
                self.damage(r, L_TASKBAR);
            }
        }

        if self.tooltip.is_none() && !self.tooltip_suppressed && !self.start.open && self.menu.is_none() && self.now - self.hover_since > TOOLTIP_DELAY && self.buttons == 0 {
            if let Some((r, text)) = self.tip.clone() {
                let w = font::UI.width(&text) + 28;
                let s = self.screen();
                let x = (r.x + (r.w - w) / 2).clamp(8, s.w - w - 8);
                let rect = Rect::new(x, self.taskbar_rect().y - 42, w, 32);
                self.tooltip = Some(Tooltip { text, rect, alpha: 0.0, glass: Glass::default() });
                self.damage(rect.expand(glass::TOOLTIP.reach()), L_TOOLTIP);
            }
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
        self.launching_until = 0;
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

fn edge_cursor(edges: u8) -> Cursor {
    let h = edges & (EDGE_L | EDGE_R);
    let v = edges & (EDGE_T | EDGE_B);
    match (h, v) {
        (0, _) => Cursor::SizeNS,
        (_, 0) => Cursor::SizeWE,
        (EDGE_L, EDGE_T) | (EDGE_R, EDGE_B) => Cursor::SizeNWSE,
        _ => Cursor::SizeNESW,
    }
}

fn month_name(m: u8) -> &'static str {
    const NAMES: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    NAMES[(m.clamp(1, 12) - 1) as usize]
}

/// Day of the week for a Gregorian date (Sakamoto's method).
fn weekday(y: u16, m: u8, d: u8) -> &'static str {
    const NAMES: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let m = m.clamp(1, 12) as i32;
    let y = y as i32 - (m < 3) as i32;
    let dow = (y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d as i32).rem_euclid(7);
    NAMES[dow as usize]
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
    p.text_shadowed(&font::UI_BOLD, 24, 18, "Apps", TEXT, 255);
    for (i, kind) in AppKind::ALL.iter().enumerate() {
        let r = Desktop::tile_rect(i);
        if hover == Some(i) {
            p.fill_squircle(r, 12.0, TEXT, 34);
        }
        let (iw, ih, m) = assets::icon(kind.taskbar_icons().1);
        p.draw_mask(r.x + (r.w - iw) / 2, r.y + 12, iw, ih, m, ACCENT, 255);
        let label = kind.short_name();
        p.text(&font::SMALL, r.x + (r.w - font::SMALL.width(label)) / 2, r.y + 54, label, TEXT, 255);
    }
    p.fill_rect(Rect::new(20, START_H - 62, START_W - 40, 1), TEXT, 36);

    // The user: there's only the one, but it's where a real desktop puts
    // the account.
    let ur = Desktop::user_rect();
    if hover == Some(START_USER) {
        p.fill_squircle(ur, 12.0, TEXT, 34);
    }
    let avatar = Rect::new(ur.x + 8, ur.y + 6, 30, 30);
    p.fill_squircle(avatar, 15.0, ACCENT, 230);
    let (iw, ih, m) = assets::icon(icon::PERSON_20_FILLED);
    p.draw_mask(avatar.x + (avatar.w - iw) / 2, avatar.y + (avatar.h - ih) / 2, iw, ih, m, rgb(18, 40, 38), 255);
    p.text_shadowed(&font::UI_BOLD, avatar.right() + 10, ur.y + 4, "konjac", TEXT, 255);
    p.text(&font::SMALL, avatar.right() + 10, ur.y + 22, "KonjacOS 0.1.0", TEXT_DIM, 255);

    let pr = Desktop::power_rect();
    if hover == Some(START_POWER) {
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
        let (color, alpha) = if item.cmd.is_some() { (TEXT, 255) } else { (TEXT_DIM, 120) };
        p.text_shadowed(&font::UI, 20, y + (MENU_ITEM_H - font::UI.line_height()) / 2, item.label, color, alpha);
        y += MENU_ITEM_H;
    }
}

fn paint_cursor(bb: &mut Surface, clip: Rect, mx: i32, my: i32, img: &cursor::Image, frame: u16) {
    let x0 = mx - img.hot_x;
    let y0 = my - img.hot_y;
    let px = img.pixels(frame);
    let r = Rect::new(x0, y0, img.w, img.h).intersect(&clip);
    for y in r.y..r.bottom() {
        let row = ((y - y0) * img.w) as usize;
        for x in r.x..r.right() {
            let s = (row + (x - x0) as usize) * 4;
            let a = px[s + 3] as u32;
            if a != 0 {
                let i = (y * bb.w + x) as usize;
                bb.px[i] = blend(bb.px[i], rgb(px[s], px[s + 1], px[s + 2]), a);
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
        d.update_cursor();
        d.flush();
        task::sleep_ticks(1);
    }
}
