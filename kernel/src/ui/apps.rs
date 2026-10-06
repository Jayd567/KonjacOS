//! The desktop's built-in apps. Each one only knows how to paint its
//! client area (everything below the title bar) and react to clicks in
//! it; the window around it -- glass, chrome, dragging, z-order -- is the
//! desktop's job (`desktop.rs`).

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use super::assets;
use super::font::{self, Font};
use super::icon_ids as icon;
use super::surface::{rgb, Painter, Rect};
use super::sysmon::{self, Snapshot};
use crate::console;
use crate::cursor::Shape as Cursor;
use crate::doom_driver;
use crate::sync::SpinLock;

pub const TEXT: u32 = rgb(240, 243, 246);
pub const TEXT_DIM: u32 = rgb(178, 188, 196);
/// The accent colour, chosen in Settings.
pub use super::settings::accent;

static CLIPBOARD: SpinLock<String> = SpinLock::new(String::new());
/// Bumped whenever the desktop changes something on the disk, so a Files
/// window showing that folder knows to look again.
static FS_REVISION: AtomicU64 = AtomicU64::new(0);

/// The text clipboard (Notepad's Cut/Copy/Paste).
pub fn clipboard() -> String {
    CLIPBOARD.lock().clone()
}

pub fn set_clipboard(text: String) {
    *CLIPBOARD.lock() = text;
}

pub fn fs_revision() -> u64 {
    FS_REVISION.load(Ordering::Acquire)
}

pub fn bump_fs_revision() {
    FS_REVISION.fetch_add(1, Ordering::Release);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppKind {
    Terminal,
    Files,
    Notepad,
    Monitor,
    Doom,
    Sketch,
    Settings,
    About,
}

impl AppKind {
    /// Every app, in Start menu order.
    pub const ALL: [AppKind; 8] = [AppKind::Terminal, AppKind::Files, AppKind::Notepad, AppKind::Monitor, AppKind::Doom, AppKind::Sketch, AppKind::Settings, AppKind::About];

    /// A stable name for saving it in `/DESKTOP.CFG`.
    pub fn id(self) -> &'static str {
        match self {
            AppKind::Terminal => "terminal",
            AppKind::Files => "files",
            AppKind::Notepad => "notepad",
            AppKind::Monitor => "monitor",
            AppKind::Doom => "doom",
            AppKind::Sketch => "sketch",
            AppKind::Settings => "settings",
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
            AppKind::Notepad => "Notepad",
            AppKind::Monitor => "Monitor",
            AppKind::Doom => "DOOM",
            AppKind::Sketch => "Sketch",
            AppKind::Settings => "Settings",
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
            AppKind::Notepad => rgb(70, 150, 222),
            AppKind::Monitor => rgb(34, 160, 140),
            AppKind::Doom => rgb(196, 48, 40),
            AppKind::Sketch => rgb(214, 84, 150),
            AppKind::Settings => rgb(92, 102, 118),
            AppKind::About => rgb(64, 116, 214),
        }
    }

    /// `(regular, filled)` 32px taskbar icons.
    pub fn taskbar_icons(self) -> (usize, usize) {
        match self {
            AppKind::Terminal => (icon::TERMINAL_32, icon::TERMINAL_32_FILLED),
            AppKind::Files => (icon::FILES_32, icon::FILES_32_FILLED),
            AppKind::Notepad => (icon::NOTEPAD_32, icon::NOTEPAD_32_FILLED),
            AppKind::Monitor => (icon::MONITOR_32, icon::MONITOR_32_FILLED),
            AppKind::Doom => (icon::DOOM_32, icon::DOOM_32_FILLED),
            AppKind::Sketch => (icon::SKETCH_32, icon::SKETCH_32_FILLED),
            AppKind::Settings => (icon::SETTINGS_32, icon::SETTINGS_32_FILLED),
            AppKind::About => (icon::ABOUT_32, icon::ABOUT_32_FILLED),
        }
    }

    pub fn small_icon(self) -> usize {
        match self {
            AppKind::Terminal => icon::TERMINAL_20,
            AppKind::Files => icon::FILES_20,
            AppKind::Notepad => icon::NOTEPAD_20,
            AppKind::Monitor => icon::MONITOR_20,
            AppKind::Doom => icon::DOOM_20,
            AppKind::Sketch => icon::SKETCH_20,
            AppKind::Settings => icon::SETTINGS_20,
            AppKind::About => icon::ABOUT_20,
        }
    }

    pub fn create(self) -> Box<dyn App> {
        match self {
            AppKind::Terminal => Box::new(Terminal::new()),
            AppKind::Files => Box::new(Files::new()),
            AppKind::Notepad => Box::new(super::notepad::Notepad::new()),
            AppKind::Monitor => Box::new(Monitor::new()),
            AppKind::Doom => Box::new(Doom::new()),
            AppKind::Sketch => Box::new(super::sketch::Sketch::new()),
            AppKind::Settings => Box::new(super::settings::SettingsApp::new()),
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
    /// Open file `path` with whatever opens it (see [`opener`]).
    Open(String),
    /// Open file `path` in Notepad, whatever kind it is.
    Edit(String),
    /// The file or folder at `from` moved to `to`, or was deleted
    /// (`None`): shortcuts to it must follow.
    PathChanged { from: String, to: Option<String> },
    /// Close this app's window now (it has finished asking whether to
    /// save, see [`App::request_close`]).
    Close,
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
    /// Show folder `dir` (Files), with entry `select` highlighted; or
    /// section `dir` (Settings).
    fn navigate(&mut self, _dir: &str, _select: Option<&str>) {}
    /// Still starting up: the pointer shows the "working" spinner.
    fn loading(&self) -> bool {
        false
    }
    /// Whether this app has a text field with focus. While it does (and
    /// its window is in front), keys come to [`App::key`] instead of the
    /// shell.
    fn wants_keys(&self) -> bool {
        false
    }
    /// A key for the focused text field: lowercase ASCII (see
    /// `keyboard::typed_char` for what it types with `mods`), or one of
    /// `keyboard::KEY_*`; `mods` are `keyboard::MOD_*` bits.
    fn key(&mut self, _code: u8, _mods: u8) -> Reply {
        Reply::default()
    }
    /// The scroll wheel turned `notches` (positive = down) over client
    /// point `(x, y)`.
    fn wheel(&mut self, _notches: i32, _x: i32, _y: i32, _w: i32, _h: i32) -> Reply {
        Reply::default()
    }
    /// The window title, if not the app's name (Notepad's file name).
    fn title(&self) -> Option<String> {
        None
    }
    /// The window is about to close: `false` keeps it open (the app asks
    /// about unsaved work, then replies [`Action::Close`] itself).
    fn request_close(&mut self) -> bool {
        true
    }
    /// Show file `path` (Notepad only).
    fn open_path(&mut self, _path: &str) {}
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
            p.fill_rect(Rect::new(TERM_PAD + cc as i32 * cw, TERM_PAD - 6 + cr as i32 * ch + 1, 2, ch - 3), accent(), 255);
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Up,
    NewFolder,
    NewFile,
    Cut,
    Copy,
    Paste,
    Rename,
    Delete,
    /// The delete confirmation's buttons.
    ConfirmDelete,
    CancelDelete,
}

/// The toolbar buttons after Up: icon, label (shown in the status bar
/// on hover), and the gap before each one.
const FILES_TOOLS: [(Tool, usize, &str, i32); 7] = [
    (Tool::NewFolder, icon::FOLDER_ADD_20, "New folder", 0),
    (Tool::NewFile, icon::DOCUMENT_ADD_20, "New text document", 0),
    (Tool::Cut, icon::CUT_20, "Cut", 12),
    (Tool::Copy, icon::COPY_20, "Copy", 0),
    (Tool::Paste, icon::PASTE_20, "Paste", 0),
    (Tool::Rename, icon::RENAME_20, "Rename", 12),
    (Tool::Delete, icon::DELETE_20, "Delete", 0),
];

/// An entry's name being edited in place (Rename, or naming something
/// new): the text, the caret, and the selected part (typing replaces it).
pub struct NameEdit {
    pub index: usize,
    pub original: String,
    pub text: String,
    pub caret: usize,
    pub select: Option<(usize, usize)>,
    /// Whether `/` may be typed (a whole path, not just a name).
    pub allow_slash: bool,
}

impl NameEdit {
    pub fn new(index: usize, name: &str, is_dir: bool) -> Self {
        // Like other desktops, the part before the extension starts out
        // selected, so typing renames the file but keeps its type.
        let end = if is_dir { name.len() } else { name.rfind('.').filter(|&d| d > 0).unwrap_or(name.len()) };
        NameEdit { index, original: String::from(name), text: String::from(name), caret: end, select: Some((0, end)), allow_slash: false }
    }

    fn delete_selection(&mut self) -> bool {
        match self.select.take() {
            Some((a, b)) if a < b => {
                self.text.replace_range(a..b, "");
                self.caret = a;
                true
            }
            _ => false,
        }
    }

    pub fn key(&mut self, code: u8, mods: u8) {
        use crate::keyboard::*;
        match code {
            KEY_LEFT => {
                self.caret = match self.select.take() {
                    Some((a, _)) => a,
                    None => self.caret.saturating_sub(1),
                }
            }
            KEY_RIGHT => {
                self.caret = match self.select.take() {
                    Some((_, b)) => b,
                    None => (self.caret + 1).min(self.text.len()),
                }
            }
            KEY_HOME => {
                self.select = None;
                self.caret = 0;
            }
            KEY_END => {
                self.select = None;
                self.caret = self.text.len();
            }
            KEY_BACKSPACE => {
                if !self.delete_selection() && self.caret > 0 {
                    self.caret -= 1;
                    self.text.remove(self.caret);
                }
            }
            KEY_DELETE => {
                if !self.delete_selection() && self.caret < self.text.len() {
                    self.text.remove(self.caret);
                }
            }
            b'a' if mods & MOD_CTRL != 0 => self.select = Some((0, self.text.len())),
            c if (0x20..0x7f).contains(&c) && mods & (MOD_CTRL | MOD_ALT) == 0 => {
                let ch = typed_char(c, mods);
                let banned = b"\\:*?\"<>|".contains(&ch) || (ch == b'/' && !self.allow_slash);
                if banned || self.text.len() >= 120 {
                    return;
                }
                self.delete_selection();
                self.text.insert(self.caret, ch as char);
                self.caret += 1;
            }
            _ => {}
        }
    }
}

/// Browses and manages the FAT16 disk, by absolute path, so it never moves
/// the shell's working directory: open, make, rename, move, copy and
/// delete files and folders, from the toolbar, right-click menus or the
/// keyboard (Delete, F2, Ctrl+C/X/V, Enter, Backspace...).
pub struct Files {
    path: String,
    entries: Vec<Entry>,
    error: Option<&'static str>,
    selected: Option<usize>,
    hover: Option<usize>,
    hover_tool: Option<Tool>,
    /// How far the list is scrolled, in pixels.
    scroll: i32,
    /// The list's visible height, from the last layout.
    list_h: i32,
    edit: Option<NameEdit>,
    /// The entry waiting for "Delete" to be confirmed.
    confirm: Option<String>,
    /// What Copy or Cut picked up: its path, and whether it moves.
    clip: Option<(String, bool)>,
    /// A message for the status bar (an error, or what just happened).
    status: Option<String>,
    fs_rev: u64,
}

const FILES_TOOLBAR: i32 = 44;
const FILES_ROW: i32 = 30;
/// Top of the first row, below the column headings.
const FILES_LIST_TOP: i32 = FILES_TOOLBAR + 26;
const FILES_STATUS_H: i32 = 30;

impl Files {
    fn new() -> Self {
        let mut f = Files {
            path: String::from("/"),
            entries: Vec::new(),
            error: None,
            selected: None,
            hover: None,
            hover_tool: None,
            scroll: 0,
            list_h: 300,
            edit: None,
            confirm: None,
            clip: None,
            status: None,
            fs_rev: fs_revision(),
        };
        f.load();
        f
    }

    fn load(&mut self) {
        self.edit = None;
        self.confirm = None;
        self.entries.clear();
        self.selected = None;
        self.hover = None;
        self.scroll = 0;
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

    /// Re-reads the folder, keeping the view where it was and selecting
    /// `select` (by name) if it's there.
    fn reload(&mut self, select: Option<&str>) {
        let scroll = self.scroll;
        self.load();
        self.selected = select.and_then(|n| self.entries.iter().position(|e| e.name.eq_ignore_ascii_case(n)));
        self.scroll = scroll.min(self.max_scroll());
        self.reveal_selected();
    }

    fn child(&self, name: &str) -> String {
        join_path(&self.path, name)
    }

    fn up(&mut self) {
        if self.path == "/" {
            return;
        }
        let left = String::from(base_name(&self.path));
        self.path = String::from(parent_path(&self.path));
        self.reload(Some(&left));
    }

    fn row_y(&self, i: usize) -> i32 {
        FILES_LIST_TOP + i as i32 * FILES_ROW - self.scroll
    }

    fn row_at(&self, y: i32) -> Option<usize> {
        if y < FILES_LIST_TOP || y >= FILES_LIST_TOP + self.list_h {
            return None;
        }
        let i = ((y - FILES_LIST_TOP + self.scroll) / FILES_ROW) as usize;
        (i < self.entries.len()).then_some(i)
    }

    fn max_scroll(&self) -> i32 {
        (self.entries.len() as i32 * FILES_ROW - self.list_h + 6).max(0)
    }

    /// Scrolls just enough to show the selected row.
    fn reveal_selected(&mut self) {
        if let Some(i) = self.selected {
            let top = i as i32 * FILES_ROW;
            if top < self.scroll {
                self.scroll = top;
            } else if top + FILES_ROW > self.scroll + self.list_h {
                self.scroll = top + FILES_ROW - self.list_h + 4;
            }
            self.scroll = self.scroll.clamp(0, self.max_scroll());
        }
    }

    fn up_rect() -> Rect {
        Rect::new(12, 6, 36, 32)
    }

    fn tool_rect(t: Tool, w: i32) -> Rect {
        if t == Tool::Up {
            return Self::up_rect();
        }
        let total: i32 = FILES_TOOLS.iter().map(|&(_, _, _, gap)| gap + 36).sum();
        let mut x = w - 12 - total;
        for &(tool, _, _, gap) in &FILES_TOOLS {
            x += gap;
            if tool == t {
                return Rect::new(x, 6, 34, 32);
            }
            x += 36;
        }
        Rect::default()
    }

    fn confirm_rect(w: i32, h: i32) -> Rect {
        Rect::new(14, h - FILES_STATUS_H - 58, w - 28, 50)
    }

    fn confirm_buttons(w: i32, h: i32) -> (Rect, Rect) {
        let c = Self::confirm_rect(w, h);
        let no = Rect::new(c.right() - 96, c.y + 9, 84, 32);
        (Rect::new(no.x - 92, no.y, 84, 32), no)
    }

    fn enabled(&self, t: Tool) -> bool {
        let sel = self.selected.is_some();
        match t {
            Tool::Up => self.path != "/",
            Tool::NewFolder | Tool::NewFile => self.error.is_none(),
            Tool::Cut | Tool::Copy | Tool::Rename | Tool::Delete => sel,
            Tool::Paste => self.clip.is_some(),
            Tool::ConfirmDelete | Tool::CancelDelete => true,
        }
    }

    fn tool_at(&self, x: i32, y: i32, w: i32, h: i32) -> Option<Tool> {
        if self.confirm.is_some() {
            let (yes, no) = Self::confirm_buttons(w, h);
            if yes.contains(x, y) {
                return Some(Tool::ConfirmDelete);
            }
            if no.contains(x, y) {
                return Some(Tool::CancelDelete);
            }
        }
        core::iter::once(Tool::Up)
            .chain(FILES_TOOLS.iter().map(|t| t.0))
            .find(|&t| Self::tool_rect(t, w).contains(x, y) && self.enabled(t))
    }

    fn exists(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// `stem ext`, or `stem (2) ext`, `stem (3) ext`... whichever is free.
    fn free_name(&self, stem: &str, ext: &str) -> String {
        let mut n = 1;
        loop {
            let mut name = String::from(stem);
            if n > 1 {
                let _ = write!(name, " ({n})");
            }
            name.push_str(ext);
            if !self.exists(&name) {
                return name;
            }
            n += 1;
        }
    }

    fn fail(&mut self, what: &str, e: &str) -> Reply {
        let mut s = String::from(what);
        s.push_str(": ");
        s.push_str(e);
        self.status = Some(s);
        Reply::repaint(true)
    }

    /// Double-click / "Open": into a folder, or hand a file to whatever
    /// opens it.
    fn open(&mut self, i: usize) -> Reply {
        let path = self.child(&self.entries[i].name);
        if self.entries[i].is_dir {
            self.path = path;
            self.load();
            return Reply::repaint(true);
        }
        if opener(&path).is_none() {
            self.status = Some(String::from("KonjacOS doesn't know how to open this kind of file."));
            return Reply::repaint(true);
        }
        Reply { repaint: true, action: Some(Action::Open(path)), ..Reply::default() }
    }

    fn new_item(&mut self, dir: bool) -> Reply {
        self.commit_edit();
        let name = if dir { self.free_name("New folder", "") } else { self.free_name("New text document", ".txt") };
        let path = self.child(&name);
        let result = if dir { crate::fat16::create_dir(&path) } else { crate::fat16::write_file(&path, b"") };
        if let Err(e) = result {
            return self.fail("Couldn't create it", e);
        }
        bump_fs_revision();
        self.fs_rev = fs_revision();
        self.reload(Some(&name));
        self.status = None;
        self.start_rename();
        Reply::repaint(true)
    }

    fn start_rename(&mut self) {
        if let Some(i) = self.selected {
            self.confirm = None;
            self.edit = Some(NameEdit::new(i, &self.entries[i].name, self.entries[i].is_dir));
        }
    }

    /// Finishes an in-place rename; returns what the desktop needs to hear.
    fn commit_edit(&mut self) -> Option<Action> {
        let edit = self.edit.take()?;
        let new = edit.text.trim();
        if new.is_empty() || new == edit.original {
            return None;
        }
        let (from, to) = (self.child(&edit.original), self.child(new));
        if let Err(e) = crate::fat16::rename(&from, &to) {
            self.fail("Couldn't rename it", e);
            return None;
        }
        let new = String::from(new);
        self.follow_clip(&from, Some(&to));
        bump_fs_revision();
        self.fs_rev = fs_revision();
        self.reload(Some(&new));
        self.status = None;
        Some(Action::PathChanged { from, to: Some(to) })
    }

    /// Keeps what Copy or Cut picked up pointing at it after it (or the
    /// folder it's in) is renamed, or forgets it once it's deleted.
    fn follow_clip(&mut self, from: &str, to: Option<&str>) {
        let Some((path, _)) = &self.clip else { return };
        let inside = path.len() > from.len() && path[..from.len()].eq_ignore_ascii_case(from) && path.as_bytes()[from.len()] == b'/';
        if !path.eq_ignore_ascii_case(from) && !inside {
            return;
        }
        match to {
            Some(to) => {
                let mut moved = String::from(to);
                moved.push_str(&path[from.len()..]);
                if let Some(clip) = &mut self.clip {
                    clip.0 = moved;
                }
            }
            None => self.clip = None,
        }
    }

    fn ask_delete(&mut self) -> Reply {
        if let Some(i) = self.selected {
            self.edit = None;
            self.confirm = Some(self.entries[i].name.clone());
        }
        Reply::repaint(true)
    }

    fn delete(&mut self) -> Reply {
        let Some(name) = self.confirm.take() else { return Reply::default() };
        let path = self.child(&name);
        if let Err(e) = crate::fat16::remove(&path) {
            return self.fail("Couldn't delete it", e);
        }
        self.follow_clip(&path, None);
        bump_fs_revision();
        self.fs_rev = fs_revision();
        let next = self.selected.and_then(|i| self.entries.get(i + 1).or(i.checked_sub(1).and_then(|j| self.entries.get(j)))).map(|e| e.name.clone());
        self.reload(next.as_deref());
        let mut s = String::from("Deleted ");
        s.push_str(&name);
        self.status = Some(s);
        Reply { repaint: true, action: Some(Action::PathChanged { from: path, to: None }), ..Reply::default() }
    }

    fn pick_up(&mut self, cut: bool) -> Reply {
        if let Some(i) = self.selected {
            let name = self.entries[i].name.clone();
            self.clip = Some((self.child(&name), cut));
            let mut s = String::from(if cut { "Cut " } else { "Copied " });
            s.push_str(&name);
            s.push_str(" -- open a folder and Paste");
            self.status = Some(s);
        }
        Reply::repaint(true)
    }

    fn paste(&mut self) -> Reply {
        self.commit_edit();
        let Some((src, cut)) = self.clip.clone() else { return Reply::default() };
        let name = String::from(base_name(&src));
        let same_folder = parent_path(&src).eq_ignore_ascii_case(&self.path);
        let mut reply = Reply::repaint(true);
        let dst_name = if cut {
            if same_folder {
                self.clip = None;
                return reply; // Already here.
            }
            if self.exists(&name) {
                return self.fail("Couldn't move it", "something with that name is already here");
            }
            name
        } else if self.exists(&name) {
            let (stem, ext) = split_ext(&name, crate::fat16::stat_path(&src).is_ok_and(|(d, _)| d));
            let mut stem = String::from(stem);
            stem.push_str(" - Copy");
            self.free_name(&stem, ext)
        } else {
            name
        };
        let dst = self.child(&dst_name);
        let result = if cut { crate::fat16::rename(&src, &dst) } else { crate::fat16::copy(&src, &dst) };
        if let Err(e) = result {
            return self.fail(if cut { "Couldn't move it" } else { "Couldn't copy it" }, e);
        }
        if cut {
            self.clip = None;
            reply.action = Some(Action::PathChanged { from: src, to: Some(dst) });
        }
        bump_fs_revision();
        self.fs_rev = fs_revision();
        self.reload(Some(&dst_name));
        self.status = None;
        reply
    }

    fn tool(&mut self, t: Tool) -> Reply {
        match t {
            Tool::Up => {
                self.commit_edit();
                self.up();
                Reply::repaint(true)
            }
            Tool::NewFolder => self.new_item(true),
            Tool::NewFile => self.new_item(false),
            Tool::Cut => self.pick_up(true),
            Tool::Copy => self.pick_up(false),
            Tool::Paste => self.paste(),
            Tool::Rename => {
                self.start_rename();
                Reply::repaint(true)
            }
            Tool::Delete => self.ask_delete(),
            Tool::ConfirmDelete => self.delete(),
            Tool::CancelDelete => {
                self.confirm = None;
                Reply::repaint(true)
            }
        }
    }

    fn move_selection(&mut self, to: usize) -> Reply {
        if !self.entries.is_empty() {
            self.selected = Some(to.min(self.entries.len() - 1));
            self.reveal_selected();
        }
        Reply::repaint(true)
    }

    fn status_text(&self, out: &mut String) {
        out.clear();
        if let Some(s) = &self.status {
            out.push_str(s);
        } else if let Some(t) = self.hover_tool.filter(|&t| t != Tool::Up) {
            out.push_str(FILES_TOOLS.iter().find(|x| x.0 == t).map_or("", |x| x.2));
        } else if let Some(i) = self.selected {
            let e = &self.entries[i];
            out.push_str(&e.name);
            if !e.is_dir {
                let mut size = String::new();
                human_size(e.size, &mut size);
                out.push_str("  -  ");
                out.push_str(&size);
            }
        } else {
            let n = self.entries.len();
            let _ = write!(out, "{n} item{}", if n == 1 { "" } else { "s" });
        }
    }
}

/// `dir` + `/` + `name`.
pub fn join_path(dir: &str, name: &str) -> String {
    let mut p = String::from(dir);
    if !p.ends_with('/') {
        p.push('/');
    }
    p.push_str(name);
    p
}

/// The folder `path` is in (`/` for something in the root).
pub fn parent_path(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

pub fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `("notes", ".txt")` for `notes.txt`; a folder has no extension.
fn split_ext(name: &str, is_dir: bool) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// What opens a file.
pub enum Opener {
    Notepad,
    /// A shell command (running a program).
    Shell(String),
    Doom,
}

fn is_text(lower: &str) -> bool {
    [".txt", ".md", ".cfg", ".log", ".ini", ".c", ".h", ".rs", ".sh", ".py", ".json", ".csv"].iter().any(|e| lower.ends_with(e))
}

/// How KonjacOS opens file `path`, if it knows: text in Notepad, programs
/// in the Terminal, a WAD starts DOOM.
pub fn opener(path: &str) -> Option<Opener> {
    let lower = path.to_ascii_lowercase();
    if is_text(&lower) {
        Some(Opener::Notepad)
    } else if lower.ends_with(".exe") || lower.ends_with(".elf") || lower.ends_with(".bin") {
        let mut cmd = String::from("run ");
        cmd.push_str(path);
        cmd.push('\n');
        Some(Opener::Shell(cmd))
    } else if lower.ends_with(".wad") {
        Some(Opener::Doom)
    } else {
        None
    }
}

pub fn file_icon(name: &str) -> usize {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".exe") || lower.ends_with(".elf") || lower.ends_with(".bin") {
        icon::APP_20
    } else if is_text(&lower) {
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

/// A one-line text field's contents with its caret and selection, for
/// painting (shared by Files' rename box and Notepad's name box).
pub fn paint_field(p: &mut Painter, r: Rect, text: &str, caret: usize, select: Option<(usize, usize)>) {
    p.fill_squircle(r, 8.0, rgb(0, 0, 0), 150);
    p.fill_rect(Rect::new(r.x + 8, r.bottom() - 2, r.w - 16, 2), accent(), 255);
    let f = &font::UI;
    let tx = r.x + 8;
    let ty = r.y + (r.h - f.line_height()) / 2;
    let x_at = |i: usize| tx + f.width(&text[..i.min(text.len())]);
    if let Some((a, b)) = select.filter(|(a, b)| a < b) {
        p.fill_rect(Rect::new(x_at(a), r.y + 5, x_at(b) - x_at(a), r.h - 10), accent(), 110);
    }
    let mut fp = p.clipped(Rect::new(r.x + 4, r.y, r.w - 8, r.h));
    fp.text(f, tx, ty, text, TEXT, 255);
    fp.fill_rect(Rect::new(x_at(caret), r.y + 6, 2, r.h - 12), accent(), 255);
}

impl App for Files {
    fn client_size(&self) -> (i32, i32) {
        (660, 460)
    }

    fn min_size(&self) -> (i32, i32) {
        (520, 260)
    }

    fn resized(&mut self, _w: i32, h: i32) {
        self.list_h = h - FILES_LIST_TOP - FILES_STATUS_H - 14;
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn tick(&mut self) -> Option<Rect> {
        // Something else changed the disk (Notepad saved, another
        // window): show it.
        let rev = fs_revision();
        if rev == self.fs_rev || self.edit.is_some() {
            return None;
        }
        self.fs_rev = rev;
        let keep = self.selected.map(|i| self.entries[i].name.clone());
        self.reload(keep.as_deref());
        Some(Rect::new(0, 0, i32::MAX / 4, i32::MAX / 4))
    }

    fn paint(&mut self, p: &mut Painter, w: i32, h: i32) {
        for t in core::iter::once((Tool::Up, icon::ARROW_UP_20, "", 0)).chain(FILES_TOOLS.iter().copied()) {
            let r = Self::tool_rect(t.0, w);
            let on = self.enabled(t.0);
            if on && self.hover_tool == Some(t.0) {
                p.fill_squircle(r, 9.0, TEXT, 34);
            }
            let (iw, ih, m) = assets::icon(t.1);
            p.draw_mask(r.x + (r.w - iw) / 2, r.y + (r.h - ih) / 2, iw, ih, m, TEXT, if on { 230 } else { 80 });
        }

        let pill = Rect::new(56, 6, Self::tool_rect(Tool::NewFolder, w).x - 64, 32);
        p.fill_squircle(pill, 10.0, rgb(0, 0, 0), 46);
        let (iw, ih, m) = assets::icon(icon::HARD_DRIVE_20);
        p.draw_mask(pill.x + 10, pill.y + 6, iw, ih, m, TEXT_DIM, 255);
        p.clipped(pill.expand(-4)).text(&font::UI, pill.x + 38, pill.y + 7, &self.path, TEXT, 255);

        let list = Rect::new(8, FILES_TOOLBAR + 4, w - 16, h - FILES_TOOLBAR - FILES_STATUS_H - 8);
        well(p, list);
        p.text(&font::SMALL, 22, FILES_TOOLBAR + 10, "NAME", TEXT_DIM, 200);
        right_text(p, &font::SMALL, w - 24, FILES_TOOLBAR + 10, "SIZE", TEXT_DIM, 200);

        let mut status = String::new();
        self.status_text(&mut status);
        let sy = h - FILES_STATUS_H + (FILES_STATUS_H - font::SMALL.line_height()) / 2 - 4;
        let warn = self.status.as_ref().is_some_and(|s| s.starts_with("Couldn't") || s.starts_with("KonjacOS doesn't"));
        let mut sx = 18;
        if warn {
            let (iw, ih, m) = assets::icon(icon::WARNING_20);
            p.draw_mask(sx, sy - 3, iw, ih, m, rgb(255, 196, 92), 255);
            sx += 26;
        }
        p.clipped(Rect::new(0, h - FILES_STATUS_H - 4, w - 16, FILES_STATUS_H)).text(&font::SMALL, sx, sy, &status, if warn { TEXT } else { TEXT_DIM }, 255);
        if let Some((src, cut)) = &self.clip {
            let mut tag = String::from(if *cut { "To move: " } else { "To copy: " });
            tag.push_str(base_name(src));
            right_text(p, &font::SMALL, w - 22, sy, &tag, accent(), 255);
        }

        if let Some(e) = self.error {
            p.text(&font::UI, 22, FILES_TOOLBAR + 40, e, TEXT_DIM, 255);
            return;
        }
        if self.entries.is_empty() {
            p.text(&font::UI, 22, FILES_TOOLBAR + 40, "This folder is empty.", TEXT_DIM, 255);
        }
        let mut size = String::new();
        let mut list_p = p.clipped(Rect::new(0, FILES_LIST_TOP - 2, w, self.list_h + 2));
        let first = (self.scroll / FILES_ROW) as usize;
        let last = ((self.scroll + self.list_h) / FILES_ROW + 1) as usize;
        for (i, e) in self.entries.iter().enumerate().take(last.min(self.entries.len())).skip(first) {
            let y = self.row_y(i);
            let row = Rect::new(14, y, w - 28, FILES_ROW - 2);
            let cut = self.clip.as_ref().is_some_and(|(p, c)| *c && base_name(p) == e.name && parent_path(p) == self.path);
            if self.selected == Some(i) {
                list_p.fill_squircle(row, 9.0, accent(), 60);
            } else if self.hover == Some(i) {
                list_p.fill_squircle(row, 9.0, TEXT, 22);
            }
            let ic = if e.is_dir { icon::FOLDER_20_FILLED } else { file_icon(&e.name) };
            let (iw, ih, m) = assets::icon(ic);
            let fade = if cut { 120 } else { 255 };
            list_p.draw_mask(24, y + 4, iw, ih, m, if e.is_dir { accent() } else { TEXT_DIM }, fade);
            match &self.edit {
                Some(ed) if ed.index == i => {
                    let field = Rect::new(48, y + 1, (w - 200).min(360), FILES_ROW - 4);
                    paint_field(&mut list_p, field, &ed.text, ed.caret, ed.select);
                }
                _ => {
                    list_p.text(&font::UI, 54, y + 6, &e.name, TEXT, fade);
                }
            }
            if !e.is_dir {
                human_size(e.size, &mut size);
                right_text(&mut list_p, &font::UI, w - 24, y + 6, &size, TEXT_DIM, 255);
            }
        }
        // A thin scroll indicator when the list doesn't fit.
        let max = self.max_scroll();
        if max > 0 {
            let track = self.list_h - 8;
            let total = track + max;
            let th = (track * track / total).max(24);
            let ty = FILES_LIST_TOP + 2 + (track - th) * self.scroll / max;
            p.fill_squircle(Rect::new(w - 15, ty, 4, th), 2.0, TEXT, 90);
        }

        if let Some(name) = &self.confirm {
            let c = Self::confirm_rect(w, h);
            p.fill_squircle(c, 14.0, rgb(10, 12, 16), 225);
            let (iw, ih, m) = assets::icon(icon::DELETE_20);
            p.draw_mask(c.x + 14, c.y + (c.h - ih) / 2, iw, ih, m, rgb(255, 120, 110), 255);
            let mut q = String::from("Delete \"");
            q.push_str(name);
            q.push_str("\"?");
            let (yes, no) = Self::confirm_buttons(w, h);
            let mut tp = p.clipped(Rect::new(c.x, c.y, yes.x - c.x - 8, c.h));
            tp.text(&font::UI_BOLD, c.x + 44, c.y + 7, &q, TEXT, 255);
            tp.text(&font::SMALL, c.x + 44, c.y + 27, "It's removed from the disk for good.", TEXT_DIM, 255);
            for (r, label, t, color) in [(yes, "Delete", Tool::ConfirmDelete, rgb(220, 60, 60)), (no, "Cancel", Tool::CancelDelete, TEXT)] {
                let hot = self.hover_tool == Some(t);
                if t == Tool::ConfirmDelete {
                    p.fill_squircle(r, 10.0, color, if hot { 255 } else { 220 });
                } else {
                    p.fill_squircle(r, 10.0, color, if hot { 50 } else { 30 });
                }
                p.text(&font::UI, r.x + (r.w - font::UI.width(label)) / 2, r.y + (r.h - font::UI.line_height()) / 2, label, TEXT, 255);
            }
        }
    }

    fn mouse(&mut self, ev: MouseEvent, x: i32, y: i32, w: i32, h: i32) -> Reply {
        match ev {
            MouseEvent::Move => {
                let hover = if self.confirm.is_some() { None } else { self.row_at(y) };
                let tool = self.tool_at(x, y, w, h);
                let changed = hover != self.hover || tool != self.hover_tool;
                self.hover = hover;
                self.hover_tool = tool;
                Reply::repaint(changed)
            }
            MouseEvent::Down | MouseEvent::DoubleClick => {
                if let Some(t) = self.tool_at(x, y, w, h) {
                    self.status = None;
                    let mut reply = self.tool(t);
                    self.hover_tool = self.tool_at(x, y, w, h);
                    reply.repaint = true;
                    return reply;
                }
                if self.confirm.is_some() {
                    return Reply::default();
                }
                let row = self.row_at(y);
                // Clicking inside the name being edited keeps editing.
                if let (Some(ed), Some(i)) = (&self.edit, row) {
                    if ed.index == i && x < 48 + (w - 200).min(360) {
                        return Reply::default();
                    }
                }
                let action = self.commit_edit();
                self.status = None;
                let mut reply = Reply::repaint(true);
                reply.action = action;
                if ev == MouseEvent::DoubleClick && reply.action.is_none() {
                    if let Some(i) = row {
                        return self.open(i);
                    }
                }
                self.selected = row;
                reply
            }
            MouseEvent::Drag | MouseEvent::Up => Reply::default(),
        }
    }

    fn wheel(&mut self, notches: i32, _x: i32, _y: i32, _w: i32, _h: i32) -> Reply {
        let s = (self.scroll + notches * 3 * FILES_ROW).clamp(0, self.max_scroll());
        let changed = s != self.scroll;
        self.scroll = s;
        Reply::repaint(changed)
    }

    fn cursor(&self, x: i32, y: i32, w: i32, h: i32) -> Cursor {
        match self.tool_at(x, y, w, h) {
            Some(Tool::Up) => Cursor::Up,
            Some(_) => Cursor::Hand,
            None => {
                let editing = self.edit.as_ref().is_some_and(|ed| self.row_at(y) == Some(ed.index) && x >= 48 && x < 48 + (w - 200).min(360));
                if editing {
                    Cursor::Text
                } else {
                    Cursor::Arrow
                }
            }
        }
    }

    fn context_menu(&mut self, _x: i32, y: i32, _w: i32, _h: i32) -> Vec<ContextItem> {
        if self.confirm.is_some() {
            return Vec::new();
        }
        self.commit_edit();
        self.selected = self.row_at(y);
        let paste = self.clip.is_some().then_some(7);
        match self.selected {
            Some(i) => {
                let e = &self.entries[i];
                let mut v = alloc::vec![("Open", (e.is_dir || opener(&e.name).is_some()).then_some(0))];
                if !e.is_dir {
                    v.push(("Edit in Notepad", Some(10)));
                }
                v.extend_from_slice(&[("", None), ("Cut", Some(5)), ("Copy", Some(6))]);
                if e.is_dir {
                    v.push(("Paste Into Folder", paste.map(|_| 11)));
                }
                v.extend_from_slice(&[("", None), ("Rename", Some(8)), ("Delete", Some(9)), ("", None), ("Create Shortcut", Some(3))]);
                v
            }
            None => alloc::vec![
                ("New Folder", Some(12)),
                ("New Text Document", Some(13)),
                ("", None),
                ("Paste", paste),
                ("", None),
                ("Up One Level", (self.path != "/").then_some(1)),
                ("Refresh", Some(2)),
            ],
        }
    }

    fn context_cmd(&mut self, id: u32) -> Reply {
        self.status = None;
        match (id, self.selected) {
            (0, Some(i)) => self.open(i),
            (1, _) => self.tool(Tool::Up),
            (3, Some(i)) => {
                let (path, is_dir) = (self.child(&self.entries[i].name), self.entries[i].is_dir);
                Reply { action: Some(Action::Shortcut { path, is_dir }), ..Reply::default() }
            }
            (5, _) => self.tool(Tool::Cut),
            (6, _) => self.tool(Tool::Copy),
            (7, _) => self.tool(Tool::Paste),
            (8, _) => self.tool(Tool::Rename),
            (9, _) => self.tool(Tool::Delete),
            (10, Some(i)) => Reply { action: Some(Action::Edit(self.child(&self.entries[i].name))), ..Reply::default() },
            (11, Some(i)) => {
                // Paste into the selected folder: step in, paste, step out.
                let name = self.entries[i].name.clone();
                let back = self.path.clone();
                self.path = self.child(&name);
                self.load();
                let reply = self.paste();
                let failed = self.status.clone();
                self.path = back;
                self.reload(Some(&name));
                self.status = failed;
                reply
            }
            (12, _) => self.tool(Tool::NewFolder),
            (13, _) => self.tool(Tool::NewFile),
            _ => {
                let keep = self.selected.map(|i| self.entries[i].name.clone());
                self.reload(keep.as_deref());
                Reply::repaint(true)
            }
        }
    }

    fn navigate(&mut self, dir: &str, select: Option<&str>) {
        self.commit_edit();
        self.path = String::from(dir);
        self.load();
        self.selected = select.and_then(|name| self.entries.iter().position(|e| e.name.eq_ignore_ascii_case(name)));
        self.reveal_selected();
    }

    fn wants_keys(&self) -> bool {
        true
    }

    fn key(&mut self, code: u8, mods: u8) -> Reply {
        use crate::keyboard::*;
        let ctrl = mods & MOD_CTRL != 0;
        if self.confirm.is_some() {
            return match code {
                KEY_ENTER => self.delete(),
                KEY_ESC => self.tool(Tool::CancelDelete),
                _ => Reply::default(),
            };
        }
        if let Some(ed) = &mut self.edit {
            return match code {
                KEY_ENTER => Reply { repaint: true, action: self.commit_edit(), ..Reply::default() },
                KEY_ESC => {
                    self.edit = None;
                    Reply::repaint(true)
                }
                _ => {
                    ed.key(code, mods);
                    Reply::repaint(true)
                }
            };
        }
        let n = self.entries.len();
        let cur = self.selected;
        self.status = None;
        match code {
            KEY_DOWN => self.move_selection(cur.map_or(0, |c| c + 1)),
            KEY_UP => self.move_selection(cur.map_or(n.saturating_sub(1), |c| c.saturating_sub(1))),
            KEY_HOME => self.move_selection(0),
            KEY_END => self.move_selection(n.saturating_sub(1)),
            KEY_PAGE_DOWN => self.move_selection(cur.map_or(0, |c| c + (self.list_h / FILES_ROW) as usize)),
            KEY_PAGE_UP => self.move_selection(cur.map_or(0, |c| c.saturating_sub((self.list_h / FILES_ROW) as usize))),
            KEY_ENTER => match cur {
                Some(i) => self.open(i),
                None => Reply::default(),
            },
            KEY_BACKSPACE => self.tool(Tool::Up),
            KEY_DELETE if cur.is_some() => self.ask_delete(),
            KEY_F2 if cur.is_some() => self.tool(Tool::Rename),
            KEY_F5 => self.context_cmd(2),
            KEY_ESC => {
                self.selected = None;
                Reply::repaint(true)
            }
            b'n' if ctrl => self.new_item(mods & MOD_SHIFT != 0),
            b'x' if ctrl => self.tool(Tool::Cut),
            b'c' if ctrl => self.tool(Tool::Copy),
            b'v' if ctrl => self.tool(Tool::Paste),
            b'a'..=b'z' | b'0'..=b'9' if mods & (MOD_CTRL | MOD_ALT) == 0 => {
                // Jump to the next entry starting with that character.
                let from = cur.map_or(0, |c| c + 1);
                match (0..n).map(|k| (from + k) % n).find(|&i| self.entries[i].name.as_bytes()[0].to_ascii_lowercase() == code) {
                    Some(i) => self.move_selection(i),
                    None => Reply::default(),
                }
            }
            _ => Reply::default(),
        }
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
            p.fill_rect(Rect::new(x0, graph.bottom() - bh, (x1 - x0 - 1).max(1), bh), accent(), 200);
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
            p.fill_squircle(Rect::new(bar.x, bar.y, fill.max(10), bar.h), 5.0, accent(), 230);
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
            let color = if t.state == "running" { accent() } else { TEXT_DIM };
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
        let ver = concat!("Version ", env!("CARGO_PKG_VERSION"));
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
        p.text(&font::UI, link.x + 4, link.y + 3, ABOUT_LINK, accent(), 255);
        if self.link_hover {
            p.fill_rect(Rect::new(link.x + 4, link.y + 20, link.w - 8, 1), accent(), 255);
        }
    }
}
