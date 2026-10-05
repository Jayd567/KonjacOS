//! The desktop's built-in apps. Each one only knows how to paint its
//! client area (everything below the title bar) and react to clicks in
//! it; the window around it -- glass, chrome, dragging, z-order -- is the
//! desktop's job (`desktop.rs`).

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::Ordering;

use super::assets;
use super::font::{self, Font};
use super::icon_ids as icon;
use super::surface::{rgb, Painter, Rect};
use super::sysmon::{self, Snapshot};
use crate::console;
use crate::cursor::Shape as Cursor;
use crate::doom_driver;

pub const TEXT: u32 = rgb(240, 243, 246);
pub const TEXT_DIM: u32 = rgb(178, 188, 196);
pub const ACCENT: u32 = rgb(120, 214, 196);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppKind {
    Terminal,
    Files,
    Monitor,
    Doom,
    Sketch,
    About,
}

impl AppKind {
    /// Every app, in Start menu order.
    pub const ALL: [AppKind; 6] = [AppKind::Terminal, AppKind::Files, AppKind::Monitor, AppKind::Doom, AppKind::Sketch, AppKind::About];

    /// A stable name for saving it in `/DESKTOP.CFG`.
    pub fn id(self) -> &'static str {
        match self {
            AppKind::Terminal => "terminal",
            AppKind::Files => "files",
            AppKind::Monitor => "monitor",
            AppKind::Doom => "doom",
            AppKind::Sketch => "sketch",
            AppKind::About => "about",
        }
    }

    pub fn from_id(id: &str) -> Option<AppKind> {
        AppKind::ALL.into_iter().find(|k| k.id() == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            AppKind::Terminal => "Terminal",
            AppKind::Files => "Files",
            AppKind::Monitor => "Monitor",
            AppKind::Doom => "DOOM",
            AppKind::Sketch => "Sketch",
            AppKind::About => "About KonjacOS",
        }
    }

    /// The name under its desktop icon and Start menu tile.
    pub fn short_name(self) -> &'static str {
        match self {
            AppKind::About => "About",
            k => k.name(),
        }
    }

    /// The colour of its desktop icon's tile.
    pub fn tint(self) -> u32 {
        match self {
            AppKind::Terminal => rgb(46, 54, 70),
            AppKind::Files => rgb(232, 164, 52),
            AppKind::Monitor => rgb(34, 160, 140),
            AppKind::Doom => rgb(196, 48, 40),
            AppKind::Sketch => rgb(214, 84, 150),
            AppKind::About => rgb(64, 116, 214),
        }
    }

    /// `(regular, filled)` 32px taskbar icons.
    pub fn taskbar_icons(self) -> (usize, usize) {
        match self {
            AppKind::Terminal => (icon::TERMINAL_32, icon::TERMINAL_32_FILLED),
            AppKind::Files => (icon::FILES_32, icon::FILES_32_FILLED),
            AppKind::Monitor => (icon::MONITOR_32, icon::MONITOR_32_FILLED),
            AppKind::Doom => (icon::DOOM_32, icon::DOOM_32_FILLED),
            AppKind::Sketch => (icon::SKETCH_32, icon::SKETCH_32_FILLED),
            AppKind::About => (icon::ABOUT_32, icon::ABOUT_32_FILLED),
        }
    }

    pub fn small_icon(self) -> usize {
        match self {
            AppKind::Terminal => icon::TERMINAL_20,
            AppKind::Files => icon::FILES_20,
            AppKind::Monitor => icon::MONITOR_20,
            AppKind::Doom => icon::DOOM_20,
            AppKind::Sketch => icon::SKETCH_20,
            AppKind::About => icon::ABOUT_20,
        }
    }

    pub fn create(self) -> Box<dyn App> {
        match self {
            AppKind::Terminal => Box::new(Terminal::new()),
            AppKind::Files => Box::new(Files::new()),
            AppKind::Monitor => Box::new(Monitor::new()),
            AppKind::Doom => Box::new(Doom::new()),
            AppKind::Sketch => Box::new(super::sketch::Sketch::new()),
            AppKind::About => Box::new(About { link_hover: false }),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Move,
    Down,
    DoubleClick,
    /// The pointer moved with the button held, after a `Down` in this
    /// app -- delivered even once it leaves the client area.
    Drag,
    /// The button was released, after a `Down` in this app.
    Up,
}

/// Something an app asks the desktop to do on its behalf.
pub enum Action {
    /// Type `command` into the shell and bring the Terminal forward.
    Shell(String),
    /// Put a shortcut to `path` (a folder if `is_dir`) on the desktop.
    Shortcut { path: String, is_dir: bool },
}

#[derive(Default)]
pub struct Reply {
    /// Repaint the whole client area...
    pub repaint: bool,
    /// ...or just this part of it (client coordinates).
    pub damage: Option<Rect>,
    pub action: Option<Action>,
}

impl Reply {
    pub fn repaint(repaint: bool) -> Self {
        Reply { repaint, ..Reply::default() }
    }
}

/// A right-click menu entry an app offers: its label and the id passed
/// back to [`App::context_cmd`] (`None` = shown greyed out). An empty
/// label is a separator.
pub type ContextItem = (&'static str, Option<u32>);

pub trait App {
    /// Initial client-area size.
    fn client_size(&self) -> (i32, i32);
    fn paint(&mut self, p: &mut Painter, w: i32, h: i32);
    fn mouse(&mut self, _ev: MouseEvent, _x: i32, _y: i32, _w: i32, _h: i32) -> Reply {
        Reply::default()
    }
    /// Called every frame; returns the part of the client area (in client
    /// coordinates) whose content changed, if any.
    fn tick(&mut self) -> Option<Rect> {
        None
    }
    fn resized(&mut self, _w: i32, _h: i32) {}
    fn closed(&mut self) {}
    /// A part of the client area this app always paints fully opaque, so
    /// the desktop can skip compositing anything underneath it.
    fn opaque_rect(&self, _w: i32, _h: i32) -> Option<Rect> {
        None
    }
    /// Whether dragging the window's edges resizes it.
    fn resizable(&self) -> bool {
        true
    }
    /// The smallest client area resizing may shrink it to.
    fn min_size(&self) -> (i32, i32) {
        (320, 200)
    }
    /// The pointer to show over client point `(x, y)`.
    fn cursor(&self, _x: i32, _y: i32, _w: i32, _h: i32) -> Cursor {
        Cursor::Arrow
    }
    /// The right-click menu for client point `(x, y)`; empty for none.
    /// Apps may update their selection to what was right-clicked first.
    fn context_menu(&mut self, _x: i32, _y: i32, _w: i32, _h: i32) -> Vec<ContextItem> {
        Vec::new()
    }
    /// One of this app's own menu entries was chosen.
    fn context_cmd(&mut self, _id: u32) -> Reply {
        Reply::default()
    }
    /// Show folder `dir` (Files only), with entry `select` highlighted.
    fn navigate(&mut self, _dir: &str, _select: Option<&str>) {}
    /// Still starting up: the pointer shows the "working" spinner.
    fn loading(&self) -> bool {
        false
    }
}

/// A slightly darker inset panel for content to sit on.
fn well(p: &mut Painter, r: Rect) {
    p.fill_squircle(r, 18.0, rgb(4, 8, 12), 70);
}

fn right_text(p: &mut Painter, f: &Font, right: i32, y: i32, s: &str, color: u32, alpha: u8) {
    p.text(f, right - f.width(s), y, s, color, alpha);
}

// --- Terminal -----------------------------------------------------------

/// Shows the shell's console grid (see `console.rs`'s grid mode) in an
/// anti-aliased monospace font. Typing goes to the shell whenever DOOM
/// doesn't have focus -- the keyboard driver delivers it straight there.
pub struct Terminal {
    cols: u64,
    rows: u64,
    text: Vec<u8>,
    cursor: (u64, u64, bool),
    revision: u64,
}

const TERM_PAD: i32 = 24;

impl Terminal {
    pub const COLS: u64 = 88;
    pub const ROWS: u64 = 26;

    fn new() -> Self {
        Terminal { cols: 0, rows: 0, text: Vec::new(), cursor: (0, 0, false), revision: u64::MAX }
    }

    fn cell() -> (i32, i32) {
        (font::MONO.cell_width(), font::MONO.line_height() + 3)
    }
}

impl App for Terminal {
    fn client_size(&self) -> (i32, i32) {
        let (cw, ch) = Self::cell();
        (Self::COLS as i32 * cw + 2 * TERM_PAD, Self::ROWS as i32 * ch + 2 * TERM_PAD - 6)
    }

    fn tick(&mut self) -> Option<Rect> {
        let con = console::CONSOLE.lock();
        if con.revision() == self.revision {
            return None;
        }
        self.revision = con.revision();
        let (cols, rows, grid, cc, cr, on) = con.grid()?;
        // Only the rows that actually changed (and the cursor's old and
        // new rows) need repainting -- a blinking cursor shouldn't redraw
        // the whole window.
        let mut changed: Option<(u64, u64)> = None;
        let mut mark = |r: u64| changed = Some(changed.map_or((r, r), |(a, b)| (a.min(r), b.max(r))));
        if cols != self.cols || rows != self.rows {
            mark(0);
            mark(rows.saturating_sub(1));
        } else {
            for r in 0..rows {
                let range = (r * cols) as usize..((r + 1) * cols) as usize;
                if grid[range.clone()] != self.text[range] {
                    mark(r);
                }
            }
        }
        if (cc, cr, on) != self.cursor {
            mark(self.cursor.1.min(rows.saturating_sub(1)));
            mark(cr);
        }
        self.cols = cols;
        self.rows = rows;
        self.text.clear();
        self.text.extend_from_slice(grid);
        self.cursor = (cc, cr, on);
        let (first, last) = changed?;
        let ch = Self::cell().1;
        let y0 = TERM_PAD - 6 + first as i32 * ch;
        Some(Rect::new(0, y0 - 2, i32::MAX / 4, (last - first + 1) as i32 * ch + 4))
    }

    fn resized(&mut self, w: i32, h: i32) {
        let (cw, ch) = Self::cell();
        let cols = ((w - 2 * TERM_PAD) / cw).max(20) as u64;
        let rows = ((h - 2 * TERM_PAD + 6) / ch).max(5) as u64;
        console::CONSOLE.lock().resize_grid(cols, rows);
    }

    fn paint(&mut self, p: &mut Painter, w: i32, h: i32) {
        well(p, Rect::new(12, 0, w - 24, h - 12));
        let (cw, ch) = Self::cell();
        let f = &font::MONO;
        let mut line = String::new();
        for r in 0..self.rows {
            let row = &self.text[(r * self.cols) as usize..((r + 1) * self.cols) as usize];
            let Some(end) = row.iter().rposition(|&b| b != b' ') else { continue };
            line.clear();
            line.extend(row[..=end].iter().map(|&b| b as char));
            let y = TERM_PAD - 6 + r as i32 * ch;
            // Monospace: place each glyph on its own cell so columns line
            // up exactly regardless of fractional advances.
            for (c, chr) in line.chars().enumerate() {
                if chr != ' ' {
                    let mut buf = [0u8; 4];
                    p.text(f, TERM_PAD + c as i32 * cw, y, chr.encode_utf8(&mut buf), TEXT, 240);
                }
            }
        }
        let (cc, cr, on) = self.cursor;
        if on && cr < self.rows {
            p.fill_rect(Rect::new(TERM_PAD + cc as i32 * cw, TERM_PAD - 6 + cr as i32 * ch + 1, 2, ch - 3), ACCENT, 255);
        }
    }

    fn min_size(&self) -> (i32, i32) {
        let (cw, ch) = Self::cell();
        (20 * cw + 2 * TERM_PAD, 5 * ch + 2 * TERM_PAD)
    }

    fn cursor(&self, x: i32, y: i32, w: i32, h: i32) -> Cursor {
        if Rect::new(12, 0, w - 24, h - 12).contains(x, y) {
            Cursor::Text
        } else {
            Cursor::Arrow
        }
    }

    fn context_menu(&mut self, _x: i32, _y: i32, _w: i32, _h: i32) -> Vec<ContextItem> {
        alloc::vec![("Clear", Some(0)), ("Shell Commands", Some(1))]
    }

    fn context_cmd(&mut self, id: u32) -> Reply {
        let cmd = if id == 0 { "clear\n" } else { "help\n" };
        Reply { action: Some(Action::Shell(String::from(cmd))), ..Reply::default() }
    }
}

// --- Files --------------------------------------------------------------

struct Entry {
    name: String,
    is_dir: bool,
    size: u32,
}

/// Browses the FAT16 disk read-only, by absolute path, so it never moves
/// the shell's working directory. Double-clicking a text file `cat`s it in
/// the Terminal; double-clicking a program `run`s it there.
pub struct Files {
    path: String,
    entries: Vec<Entry>,
    error: Option<&'static str>,
    selected: Option<usize>,
    hover: Option<usize>,
    up_hover: bool,
}

const FILES_TOOLBAR: i32 = 44;
const FILES_ROW: i32 = 30;

impl Files {
    fn new() -> Self {
        let mut f = Files { path: String::from("/"), entries: Vec::new(), error: None, selected: None, hover: None, up_hover: false };
        f.load();
        f
    }

    fn load(&mut self) {
        self.entries.clear();
        self.selected = None;
        self.hover = None;
        match crate::fat16::list_dir(&self.path) {
            Ok(list) => {
                self.error = None;
                for e in list {
                    if e.name == "." || e.name == ".." {
                        continue;
                    }
                    self.entries.push(Entry { name: e.name, is_dir: e.is_dir, size: e.size });
                }
                self.entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase())));
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn child(&self, name: &str) -> String {
        let mut p = self.path.clone();
        if !p.ends_with('/') {
            p.push('/');
        }
        p.push_str(name);
        p
    }

    fn up(&mut self) {
        if self.path == "/" {
            return;
        }
        let cut = self.path.rfind('/').unwrap_or(0);
        self.path.truncate(cut.max(1));
        self.load();
    }

    fn row_at(&self, y: i32) -> Option<usize> {
        if y < FILES_TOOLBAR + 26 {
            return None;
        }
        let i = ((y - FILES_TOOLBAR - 26) / FILES_ROW) as usize;
        (i < self.entries.len()).then_some(i)
    }

    fn up_rect() -> Rect {
        Rect::new(12, 6, 36, 32)
    }

    /// Double-click / "Open": into a folder, or hand a file to the shell.
    fn open(&mut self, i: usize) -> Reply {
        let mut reply = Reply::repaint(true);
        let path = self.child(&self.entries[i].name);
        if self.entries[i].is_dir {
            self.path = path;
            self.load();
        } else if let Some(cmd) = open_command(&path) {
            reply.action = Some(Action::Shell(cmd));
        }
        reply
    }
}

/// The shell command that opens file `path`, if KonjacOS knows how:
/// text is `cat`ed, programs are `run`, a WAD starts DOOM.
pub fn open_command(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    let verb = if lower.ends_with(".txt") || lower.ends_with(".md") || lower.ends_with(".cfg") {
        "cat "
    } else if lower.ends_with(".exe") || lower.ends_with(".elf") || lower.ends_with(".bin") {
        "run "
    } else if lower.ends_with(".wad") {
        return Some(String::from("doom\n"));
    } else {
        return None;
    };
    let mut cmd = String::from(verb);
    cmd.push_str(path);
    cmd.push('\n');
    Some(cmd)
}

pub fn file_icon(name: &str) -> usize {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".exe") || lower.ends_with(".elf") || lower.ends_with(".bin") {
        icon::APP_20
    } else if lower.ends_with(".txt") || lower.ends_with(".md") || lower.ends_with(".cfg") {
        icon::DOCUMENT_TEXT_20
    } else {
        icon::DOCUMENT_20
    }
}

fn human_size(size: u32, out: &mut String) {
    out.clear();
    let _ = if size >= 1024 * 1024 {
        write!(out, "{}.{} MB", size / (1024 * 1024), size % (1024 * 1024) * 10 / (1024 * 1024))
    } else if size >= 1024 {
        write!(out, "{} KB", size / 1024)
    } else {
        write!(out, "{size} B")
    };
}

impl App for Files {
    fn client_size(&self) -> (i32, i32) {
        (560, 440)
    }

    fn paint(&mut self, p: &mut Painter, w: i32, h: i32) {
        let up = Self::up_rect();
        if self.up_hover && self.path != "/" {
            p.fill_squircle(up, 9.0, TEXT, 34);
        }
        let (iw, ih, m) = assets::icon(icon::ARROW_UP_20);
        p.draw_mask(up.x + 8, up.y + 6, iw, ih, m, TEXT, if self.path == "/" { 90 } else { 230 });

        let pill = Rect::new(56, 6, w - 68, 32);
        p.fill_squircle(pill, 10.0, rgb(0, 0, 0), 46);
        let (iw, ih, m) = assets::icon(icon::HARD_DRIVE_20);
        p.draw_mask(pill.x + 10, pill.y + 6, iw, ih, m, TEXT_DIM, 255);
        p.text(&font::UI, pill.x + 38, pill.y + 7, &self.path, TEXT, 255);

        let list = Rect::new(8, FILES_TOOLBAR + 4, w - 16, h - FILES_TOOLBAR - 12);
        well(p, list);
        p.text(&font::SMALL, 22, FILES_TOOLBAR + 10, "NAME", TEXT_DIM, 200);
        right_text(p, &font::SMALL, w - 24, FILES_TOOLBAR + 10, "SIZE", TEXT_DIM, 200);

        if let Some(e) = self.error {
            p.text(&font::UI, 22, FILES_TOOLBAR + 40, e, TEXT_DIM, 255);
            return;
        }
        if self.entries.is_empty() {
            p.text(&font::UI, 22, FILES_TOOLBAR + 40, "This folder is empty.", TEXT_DIM, 255);
        }
        let mut size = String::new();
        let mut list_p = p.sub(Rect::new(0, 0, w, list.bottom() - 4));
        for (i, e) in self.entries.iter().enumerate() {
            let y = FILES_TOOLBAR + 26 + i as i32 * FILES_ROW;
            let row = Rect::new(14, y, w - 28, FILES_ROW - 2);
            if self.selected == Some(i) {
                list_p.fill_squircle(row, 9.0, ACCENT, 60);
            } else if self.hover == Some(i) {
                list_p.fill_squircle(row, 9.0, TEXT, 22);
            }
            let ic = if e.is_dir { icon::FOLDER_20_FILLED } else { file_icon(&e.name) };
            let (iw, ih, m) = assets::icon(ic);
            list_p.draw_mask(24, y + 4, iw, ih, m, if e.is_dir { ACCENT } else { TEXT_DIM }, 255);
            list_p.text(&font::UI, 54, y + 6, &e.name, TEXT, 255);
            if !e.is_dir {
                human_size(e.size, &mut size);
                right_text(&mut list_p, &font::UI, w - 24, y + 6, &size, TEXT_DIM, 255);
            }
        }
    }

    fn mouse(&mut self, ev: MouseEvent, x: i32, y: i32, _w: i32, _h: i32) -> Reply {
        match ev {
            MouseEvent::Move => {
                let hover = self.row_at(y);
                let up_hover = Self::up_rect().contains(x, y);
                let changed = hover != self.hover || up_hover != self.up_hover;
                self.hover = hover;
                self.up_hover = up_hover;
                Reply::repaint(changed)
            }
            MouseEvent::Down => {
                if Self::up_rect().contains(x, y) {
                    self.up();
                } else {
                    self.selected = self.row_at(y);
                }
                Reply::repaint(true)
            }
            MouseEvent::DoubleClick => match self.row_at(y) {
                Some(i) => self.open(i),
                None => Reply::default(),
            },
            MouseEvent::Drag | MouseEvent::Up => Reply::default(),
        }
    }

    fn cursor(&self, x: i32, y: i32, _w: i32, _h: i32) -> Cursor {
        if Self::up_rect().contains(x, y) && self.path != "/" {
            Cursor::Up
        } else {
            Cursor::Arrow
        }
    }

    fn context_menu(&mut self, _x: i32, y: i32, _w: i32, _h: i32) -> Vec<ContextItem> {
        self.selected = self.row_at(y);
        match self.selected {
            Some(i) if self.entries[i].is_dir => alloc::vec![("Open", Some(0)), ("", None), ("Create Shortcut", Some(3))],
            Some(i) => alloc::vec![("Open", open_command(&self.entries[i].name).map(|_| 0)), ("", None), ("Create Shortcut", Some(3))],
            None => alloc::vec![("Up One Level", (self.path != "/").then_some(1)), ("Refresh", Some(2))],
        }
    }

    fn context_cmd(&mut self, id: u32) -> Reply {
        match (id, self.selected) {
            (0, Some(i)) => self.open(i),
            (1, _) => {
                self.up();
                Reply::repaint(true)
            }
            (3, Some(i)) => {
                let (path, is_dir) = (self.child(&self.entries[i].name), self.entries[i].is_dir);
                Reply { action: Some(Action::Shortcut { path, is_dir }), ..Reply::default() }
            }
            _ => {
                self.load();
                Reply::repaint(true)
            }
        }
    }

    fn navigate(&mut self, dir: &str, select: Option<&str>) {
        self.path = String::from(dir);
        self.load();
        self.selected = select.and_then(|name| self.entries.iter().position(|e| e.name.eq_ignore_ascii_case(name)));
    }
}

// --- Monitor ------------------------------------------------------------

/// Live CPU, memory and task list, straight from the sysmon snapshot.
pub struct Monitor {
    snap: Snapshot,
    seq: u64,
}

impl Monitor {
    fn new() -> Self {
        Monitor { snap: sysmon::latest(), seq: u64::MAX }
    }
}

impl App for Monitor {
    fn client_size(&self) -> (i32, i32) {
        (520, 470)
    }

    fn min_size(&self) -> (i32, i32) {
        (420, 300)
    }

    fn tick(&mut self) -> Option<Rect> {
        let seq = sysmon::SEQ.load(Ordering::Acquire);
        if seq == self.seq {
            return None;
        }
        self.seq = seq;
        self.snap = sysmon::latest();
        Some(Rect::new(0, 0, i32::MAX / 4, i32::MAX / 4))
    }

    fn paint(&mut self, p: &mut Painter, w: i32, h: i32) {
        let s = &self.snap;
        let mut buf = String::new();
        let card_w = (w - 28) / 2;

        // CPU card: big number plus a sparkline of the last minute.
        let cpu = Rect::new(8, 0, card_w, 150);
        well(p, cpu);
        p.text(&font::SMALL, cpu.x + 16, cpu.y + 14, "CPU", TEXT_DIM, 230);
        buf.clear();
        let _ = write!(buf, "{}%", s.cpu);
        p.text(&font::DISPLAY, cpu.x + 16, cpu.y + 32, &buf, TEXT, 255);
        let graph = Rect::new(cpu.x + 16, cpu.y + 82, cpu.w - 32, 52);
        p.fill_rect(Rect::new(graph.x, graph.bottom() - 1, graph.w, 1), TEXT, 40);
        let n = sysmon::HISTORY as i32;
        for (i, &v) in s.cpu_history.iter().enumerate() {
            let x0 = graph.x + i as i32 * graph.w / n;
            let x1 = graph.x + (i as i32 + 1) * graph.w / n;
            let bh = (v as i32 * graph.h / 100).max(1);
            p.fill_rect(Rect::new(x0, graph.bottom() - bh, (x1 - x0 - 1).max(1), bh), ACCENT, 200);
        }

        // Memory card.
        let mem = Rect::new(20 + card_w, 0, card_w, 150);
        well(p, mem);
        p.text(&font::SMALL, mem.x + 16, mem.y + 14, "MEMORY", TEXT_DIM, 230);
        let mib = 1024 * 1024;
        let pct = if s.mem_total > 0 { s.mem_used * 100 / s.mem_total } else { 0 };
        buf.clear();
        let _ = write!(buf, "{pct}%");
        p.text(&font::DISPLAY, mem.x + 16, mem.y + 32, &buf, TEXT, 255);
        buf.clear();
        let _ = write!(buf, "{} of {} MiB in use", s.mem_used / mib, s.mem_total / mib);
        p.text(&font::UI, mem.x + 16, mem.y + 80, &buf, TEXT_DIM, 255);
        let bar = Rect::new(mem.x + 16, mem.y + 112, mem.w - 32, 10);
        p.fill_squircle(bar, 5.0, TEXT, 40);
        let fill = (bar.w as u64 * pct / 100) as i32;
        if fill > 0 {
            p.fill_squircle(Rect::new(bar.x, bar.y, fill.max(10), bar.h), 5.0, ACCENT, 230);
        }

        // Task table.
        let table = Rect::new(8, 162, w - 16, h - 170);
        well(p, table);
        let cols = [table.x + 16, table.x + 76, table.x + 260];
        p.text(&font::SMALL, cols[0], table.y + 12, "ID", TEXT_DIM, 230);
        p.text(&font::SMALL, cols[1], table.y + 12, "TASK", TEXT_DIM, 230);
        p.text(&font::SMALL, cols[2], table.y + 12, "STATE", TEXT_DIM, 230);
        buf.clear();
        let _ = write!(buf, "up {}h {:02}m {:02}s", s.uptime_secs / 3600, s.uptime_secs / 60 % 60, s.uptime_secs % 60);
        right_text(p, &font::SMALL, table.right() - 16, table.y + 12, &buf, TEXT_DIM, 230);
        let mut tp = p.sub(Rect::new(0, 0, w, table.bottom() - 6));
        for (i, t) in s.tasks[..s.task_count].iter().enumerate() {
            let y = table.y + 36 + i as i32 * 24;
            buf.clear();
            let _ = write!(buf, "{}", t.id);
            tp.text(&font::UI, cols[0], y, &buf, TEXT_DIM, 255);
            tp.text(&font::UI, cols[1], y, t.name, TEXT, 255);
            let color = if t.state == "running" { ACCENT } else { TEXT_DIM };
            tp.text(&font::UI, cols[2], y, t.state, color, 255);
        }
    }
}

// --- DOOM ---------------------------------------------------------------

/// DOOM's window: shows the latest frame from `doom_driver::FRAME`.
/// Closing it ends the DOOM task.
pub struct Doom {
    seq: u64,
}

pub const DOOM_INSET: i32 = 8;

impl Doom {
    fn new() -> Self {
        Doom { seq: u64::MAX }
    }
}

impl App for Doom {
    fn client_size(&self) -> (i32, i32) {
        (doom_driver::WIDTH as i32 + 2 * DOOM_INSET, doom_driver::HEIGHT as i32 + DOOM_INSET)
    }

    fn tick(&mut self) -> Option<Rect> {
        let seq = doom_driver::FRAME_SEQ.load(Ordering::Acquire);
        if seq == self.seq {
            return None;
        }
        self.seq = seq;
        Some(Rect::new(DOOM_INSET, 0, doom_driver::WIDTH as i32, doom_driver::HEIGHT as i32))
    }

    fn opaque_rect(&self, w: i32, _h: i32) -> Option<Rect> {
        let fw = doom_driver::WIDTH as i32;
        (self.seq != 0 && self.seq != u64::MAX).then(|| Rect::new((w - fw) / 2, 0, fw, doom_driver::HEIGHT as i32))
    }

    fn paint(&mut self, p: &mut Painter, w: i32, _h: i32) {
        let (fw, fh) = (doom_driver::WIDTH as i32, doom_driver::HEIGHT as i32);
        let x = (w - fw) / 2;
        let frame = doom_driver::FRAME.lock();
        if self.seq == 0 || frame.len() != (fw * fh) as usize {
            drop(frame);
            p.fill_rect(Rect::new(x, 0, fw, fh), rgb(0, 0, 0), 200);
            let msg = "Starting DOOM...";
            p.text(&font::UI, x + (fw - font::UI.width(msg)) / 2, fh / 2 - 8, msg, TEXT_DIM, 255);
            return;
        }
        p.blit(x, 0, fw, fh, &frame, fw);
    }

    fn closed(&mut self) {
        if let Some(id) = doom_driver::running_task() {
            crate::task::kill(id);
        }
    }

    // DOOM renders at a fixed 640x400; a bigger window would only add
    // empty glass round it.
    fn resizable(&self) -> bool {
        false
    }

    fn loading(&self) -> bool {
        self.seq == 0 || self.seq == u64::MAX
    }

    fn cursor(&self, _x: i32, _y: i32, _w: i32, _h: i32) -> Cursor {
        if self.loading() {
            Cursor::Busy
        } else {
            Cursor::Arrow
        }
    }
}

// --- About --------------------------------------------------------------

pub struct About {
    link_hover: bool,
}

const ABOUT_LINK: &str = "Show shell commands";

impl About {
    fn link_rect(w: i32) -> Rect {
        let lw = font::UI.width(ABOUT_LINK);
        Rect::new((w - lw) / 2 - 4, 340, lw + 8, 24)
    }
}

impl App for About {
    fn client_size(&self) -> (i32, i32) {
        (400, 384)
    }

    fn resizable(&self) -> bool {
        false
    }

    fn cursor(&self, x: i32, y: i32, w: i32, _h: i32) -> Cursor {
        if Self::link_rect(w).contains(x, y) {
            Cursor::Hand
        } else {
            Cursor::Arrow
        }
    }

    fn mouse(&mut self, ev: MouseEvent, x: i32, y: i32, w: i32, _h: i32) -> Reply {
        let over = Self::link_rect(w).contains(x, y);
        match ev {
            MouseEvent::Move => {
                let changed = over != self.link_hover;
                self.link_hover = over;
                Reply::repaint(changed)
            }
            MouseEvent::Down if over => Reply { action: Some(Action::Shell(String::from("help\n"))), ..Reply::default() },
            _ => Reply::default(),
        }
    }

    fn paint(&mut self, p: &mut Painter, w: i32, _h: i32) {
        let (lw, lh, m) = assets::logo_mid();
        p.draw_mask((w - lw) / 2, 12, lw, lh, m, TEXT, 255);
        let title = "KonjacOS";
        p.text_shadowed(&font::DISPLAY, (w - font::DISPLAY.width(title)) / 2, 90, title, TEXT, 255);
        let ver = "Version 0.1.0";
        p.text(&font::UI, (w - font::UI.width(ver)) / 2, 130, ver, TEXT_DIM, 255);

        let s = sysmon::latest();
        let mut mem = String::new();
        let _ = write!(mem, "{} MiB", s.mem_total / (1024 * 1024));
        let rows: [(&str, &str); 4] = [
            ("Kernel", "x86_64, written in Rust"),
            ("Memory", &mem),
            ("Boots with", "Limine (BIOS + UEFI)"),
            ("License", "GNU AGPL v3"),
        ];
        let card = Rect::new(20, 170, w - 40, 36 * rows.len() as i32 + 16);
        well(p, card);
        for (i, (k, v)) in rows.iter().enumerate() {
            let y = card.y + 16 + i as i32 * 36;
            p.text(&font::UI, card.x + 18, y, k, TEXT_DIM, 255);
            right_text(p, &font::UI, card.right() - 18, y, v, TEXT, 255);
            if i + 1 < rows.len() {
                p.fill_rect(Rect::new(card.x + 18, y + 27, card.w - 36, 1), TEXT, 24);
            }
        }

        let link = Self::link_rect(w);
        p.text(&font::UI, link.x + 4, link.y + 3, ABOUT_LINK, ACCENT, 255);
        if self.link_hover {
            p.fill_rect(Rect::new(link.x + 4, link.y + 20, link.w - 8, 1), ACCENT, 255);
        }
    }
}
