//! Notepad: a plain-text editor for files on the FAT16 disk.
//!
//! Text is kept as lines of printable ASCII (tabs become spaces on
//! loading; a file with CRLF line endings is saved back with them). The
//! usual editing keys work -- arrows, Home/End, Page Up/Down, Shift to
//! select, Ctrl+arrows by word -- along with Ctrl+S/Shift+S/N/A/C/X/V/Z/Y,
//! mouse selection and the wheel. Closing it, or opening another file,
//! with unsaved changes asks first, in a bar across the top of the page.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use super::apps::{accent, base_name, bump_fs_revision, clipboard, paint_field, parent_path, set_clipboard, Action, App, MouseEvent, NameEdit, Reply, TEXT, TEXT_DIM};
use super::assets;
use super::font;
use super::icon_ids as icon;
use super::surface::{rgb, Painter, Rect};
use crate::cursor::Shape as Cursor;
use crate::keyboard::*;

const TOOLBAR_H: i32 = 44;
const PROMPT_H: i32 = 52;
const MAX_FILE: usize = 512 * 1024;
const UNDO_DEPTH: usize = 40;
/// Caret blink half-period, in timer ticks.
const BLINK: u64 = 53;

/// (line, column), both from 0. Columns are byte offsets -- the text is
/// ASCII, so that's also characters.
type Pos = (usize, usize);

/// What to do once a save (or "Don't save") has gone through.
#[derive(Clone)]
enum After {
    Nothing,
    Close,
    Open(String),
    New,
}

enum Prompt {
    /// Asking for a file name.
    SaveAs(NameEdit, After),
    /// "Save changes to ...?"
    Unsaved(After),
    /// The name typed into Save As is already taken.
    Replace(String, After),
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ctl {
    New,
    Save,
    SaveAs,
    Cut,
    Copy,
    Paste,
    /// Prompt buttons, left to right.
    PromptBtn(u8),
    Field,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    None,
    Typing,
    Deleting,
}

pub struct Notepad {
    path: Option<String>,
    lines: Vec<String>,
    cur: Pos,
    /// The other end of the selection, if there is one.
    anchor: Option<Pos>,
    /// The column Up/Down try to stay in.
    want_col: Option<usize>,
    /// First visible line, and first visible column.
    top: usize,
    left: usize,
    dirty: bool,
    crlf: bool,
    prompt: Option<Prompt>,
    hover: Option<Ctl>,
    dragging: bool,
    w: i32,
    h: i32,
    undo: Vec<(Vec<String>, Pos)>,
    redo: Vec<(Vec<String>, Pos)>,
    last_edit: EditKind,
    blink_on: bool,
    blink_at: u64,
}

fn cell() -> (i32, i32) {
    (font::MONO.cell_width(), font::MONO.line_height() + 4)
}

fn order(a: Pos, b: Pos) -> (Pos, Pos) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

impl Notepad {
    pub fn new() -> Self {
        Notepad {
            path: None,
            lines: alloc::vec![String::new()],
            cur: (0, 0),
            anchor: None,
            want_col: None,
            top: 0,
            left: 0,
            dirty: false,
            crlf: false,
            prompt: None,
            hover: None,
            dragging: false,
            w: 720,
            h: 480,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: EditKind::None,
            blink_on: true,
            blink_at: 0,
        }
    }

    // --- Layout ---------------------------------------------------------

    fn page_top(&self) -> i32 {
        TOOLBAR_H + if self.prompt.is_some() { PROMPT_H } else { 0 }
    }

    /// The text page (the dark well).
    fn page(&self) -> Rect {
        let top = self.page_top();
        Rect::new(8, top, self.w - 16, self.h - top - 8)
    }

    fn gutter_w(&self) -> i32 {
        let digits = (self.lines.len().max(1).ilog10() + 1).max(3) as i32;
        digits * cell().0 + 22
    }

    fn text_x(&self) -> i32 {
        self.page().x + self.gutter_w() + 6
    }

    fn text_y(&self) -> i32 {
        self.page().y + 10
    }

    fn visible_lines(&self) -> usize {
        ((self.page().h - 16) / cell().1).max(1) as usize
    }

    fn visible_cols(&self) -> usize {
        ((self.page().right() - 12 - self.text_x()) / cell().0).max(4) as usize
    }

    fn tool_rect(c: Ctl) -> Rect {
        match c {
            Ctl::New => Rect::new(12, 6, 34, 32),
            Ctl::Save => Rect::new(48, 6, 34, 32),
            Ctl::SaveAs => Rect::new(86, 6, font::UI.width("Save as...") + 20, 32),
            Ctl::Cut => Rect::new(200, 6, 34, 32),
            Ctl::Copy => Rect::new(236, 6, 34, 32),
            Ctl::Paste => Rect::new(272, 6, 34, 32),
            _ => Rect::default(),
        }
    }

    fn prompt_rect(&self) -> Rect {
        Rect::new(8, TOOLBAR_H, self.w - 16, PROMPT_H - 8)
    }

    /// The prompt's buttons, right to left from the edge: labels.
    fn prompt_buttons(&self) -> &'static [&'static str] {
        match &self.prompt {
            Some(Prompt::SaveAs(..)) => &["Save", "Cancel"],
            Some(Prompt::Unsaved(_)) => &["Save", "Don't save", "Cancel"],
            Some(Prompt::Replace(..)) => &["Replace", "Cancel"],
            Some(Prompt::Error(_)) => &["OK"],
            None => &[],
        }
    }

    fn prompt_button_rect(&self, i: usize) -> Rect {
        let labels = self.prompt_buttons();
        let r = self.prompt_rect();
        let mut x = r.right() - 8;
        for (k, label) in labels.iter().enumerate().rev() {
            let bw = font::UI.width(label) + 28;
            x -= bw;
            if k == i {
                return Rect::new(x, r.y + 6, bw, r.h - 12);
            }
            x -= 8;
        }
        Rect::default()
    }

    fn field_rect(&self) -> Rect {
        let r = self.prompt_rect();
        let right = self.prompt_button_rect(0).x - 12;
        Rect::new(r.x + 100, r.y + 7, (right - r.x - 100).max(60), r.h - 14)
    }

    fn ctl_at(&self, x: i32, y: i32) -> Option<Ctl> {
        for c in [Ctl::New, Ctl::Save, Ctl::SaveAs, Ctl::Cut, Ctl::Copy, Ctl::Paste] {
            if Self::tool_rect(c).contains(x, y) && self.enabled(c) {
                return Some(c);
            }
        }
        if self.prompt.is_some() {
            for i in 0..self.prompt_buttons().len() {
                if self.prompt_button_rect(i).contains(x, y) {
                    return Some(Ctl::PromptBtn(i as u8));
                }
            }
            if matches!(self.prompt, Some(Prompt::SaveAs(..))) && self.field_rect().contains(x, y) {
                return Some(Ctl::Field);
            }
        }
        None
    }

    fn enabled(&self, c: Ctl) -> bool {
        match c {
            Ctl::Cut | Ctl::Copy => self.selection().is_some(),
            Ctl::Paste => !clipboard().is_empty(),
            _ => true,
        }
    }

    /// The text position nearest client point `(x, y)`.
    fn pos_at(&self, x: i32, y: i32) -> Pos {
        let (cw, lh) = cell();
        let row = self.top as i64 + (y - self.text_y()).div_euclid(lh) as i64;
        let line = row.clamp(0, self.lines.len() as i64 - 1) as usize;
        let col = (self.left as i64 + (x - self.text_x() + cw / 2).div_euclid(cw) as i64).max(0) as usize;
        (line, col.min(self.lines[line].len()))
    }

    // --- Text -----------------------------------------------------------

    fn selection(&self) -> Option<(Pos, Pos)> {
        self.anchor.filter(|&a| a != self.cur).map(|a| order(a, self.cur))
    }

    fn selected_text(&self) -> String {
        let Some((s, e)) = self.selection() else { return String::new() };
        let mut out = String::new();
        for l in s.0..=e.0 {
            let line = &self.lines[l];
            let a = if l == s.0 { s.1 } else { 0 };
            let b = if l == e.0 { e.1 } else { line.len() };
            out.push_str(&line[a..b]);
            if l != e.0 {
                out.push('\n');
            }
        }
        out
    }

    /// Saves an undo step before an edit of kind `kind` -- one per word
    /// typed or run of deletes, not per keystroke.
    fn checkpoint(&mut self, kind: EditKind) {
        if kind == EditKind::None || kind != self.last_edit {
            self.undo.push((self.lines.clone(), self.cur));
            if self.undo.len() > UNDO_DEPTH {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.last_edit = kind;
        self.dirty = true;
    }

    fn delete_selection(&mut self) -> bool {
        let Some((s, e)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let tail = String::from(&self.lines[e.0][e.1..]);
        self.lines[s.0].truncate(s.1);
        self.lines[s.0].push_str(&tail);
        self.lines.drain(s.0 + 1..=e.0);
        self.cur = s;
        self.anchor = None;
        true
    }

    /// Inserts `text` (printable ASCII and newlines) at the caret,
    /// replacing the selection.
    fn insert(&mut self, text: &str) {
        self.delete_selection();
        let (l, c) = self.cur;
        let tail = String::from(&self.lines[l][c..]);
        self.lines[l].truncate(c);
        let mut line = l;
        for (k, part) in text.split('\n').enumerate() {
            if k > 0 {
                line += 1;
                self.lines.insert(line, String::new());
            }
            for b in part.bytes() {
                self.lines[line].push(if (0x20..0x7f).contains(&b) { b as char } else { '?' });
            }
        }
        let col = self.lines[line].len();
        self.lines[line].push_str(&tail);
        self.cur = (line, col);
        self.want_col = None;
    }

    fn word_left(&self, (l, c): Pos) -> Pos {
        if c == 0 {
            return if l > 0 { (l - 1, self.lines[l - 1].len()) } else { (0, 0) };
        }
        let b = self.lines[l].as_bytes();
        let mut i = c;
        while i > 0 && !is_word(b[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(b[i - 1]) {
            i -= 1;
        }
        (l, i)
    }

    fn word_right(&self, (l, c): Pos) -> Pos {
        let b = self.lines[l].as_bytes();
        if c >= b.len() {
            return if l + 1 < self.lines.len() { (l + 1, 0) } else { (l, c) };
        }
        let mut i = c;
        while i < b.len() && is_word(b[i]) {
            i += 1;
        }
        while i < b.len() && !is_word(b[i]) {
            i += 1;
        }
        (l, i)
    }

    /// Moves the caret to `to`, extending the selection if `extend`.
    fn move_to(&mut self, to: Pos, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cur);
            }
        } else {
            self.anchor = None;
        }
        self.cur = to;
        self.last_edit = EditKind::None;
    }

    /// Scrolls so the caret is in view.
    fn reveal(&mut self) {
        let vis = self.visible_lines();
        if self.cur.0 < self.top {
            self.top = self.cur.0;
        } else if self.cur.0 >= self.top + vis {
            self.top = self.cur.0 + 1 - vis;
        }
        let cols = self.visible_cols();
        if self.cur.1 < self.left {
            self.left = self.cur.1.saturating_sub(8);
        } else if self.cur.1 >= self.left + cols {
            self.left = self.cur.1 + 8 - cols;
        }
        self.blink_on = true;
        self.blink_at = crate::timer::ticks();
    }

    // --- Files ----------------------------------------------------------

    fn reset(&mut self) {
        self.lines = alloc::vec![String::new()];
        self.cur = (0, 0);
        self.anchor = None;
        self.top = 0;
        self.left = 0;
        self.dirty = false;
        self.crlf = false;
        self.undo.clear();
        self.redo.clear();
        self.last_edit = EditKind::None;
    }

    fn load(&mut self, path: &str) {
        let data = match crate::fat16::read_file(path) {
            Ok(d) => d,
            Err(e) => {
                let mut msg = String::from("Couldn't open it: ");
                msg.push_str(e);
                self.prompt = Some(Prompt::Error(msg));
                return;
            }
        };
        if data.len() > MAX_FILE || data.contains(&0) {
            self.prompt = Some(Prompt::Error(String::from("That isn't a text file Notepad can show.")));
            return;
        }
        self.reset();
        self.crlf = data.windows(2).any(|w| w == b"\r\n");
        let mut lines = alloc::vec![String::new()];
        for &b in &data {
            let line = lines.last_mut().unwrap();
            match b {
                b'\n' => lines.push(String::new()),
                b'\r' => {}
                // Tabs to the next multiple of four.
                b'\t' => {
                    let n = 4 - line.len() % 4;
                    for _ in 0..n {
                        line.push(' ');
                    }
                }
                0x20..=0x7e => line.push(b as char),
                _ => line.push('?'),
            }
        }
        self.lines = lines;
        self.path = Some(String::from(path));
        self.prompt = None;
    }

    fn contents(&self) -> Vec<u8> {
        let nl: &[u8] = if self.crlf { b"\r\n" } else { b"\n" };
        let mut out = Vec::new();
        for (i, l) in self.lines.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(nl);
            }
            out.extend_from_slice(l.as_bytes());
        }
        out
    }

    /// Carries out `after` now that the document is saved or discarded.
    fn then(&mut self, after: After) -> Reply {
        self.prompt = None;
        match after {
            After::Nothing => Reply::repaint(true),
            After::Close => Reply { repaint: true, action: Some(Action::Close), ..Reply::default() },
            After::Open(p) => {
                self.load(&p);
                Reply::repaint(true)
            }
            After::New => {
                self.reset();
                self.path = None;
                Reply::repaint(true)
            }
        }
    }

    fn save(&mut self, after: After) -> Reply {
        let Some(path) = self.path.clone() else { return self.ask_name(after) };
        match crate::fat16::write_file(&path, &self.contents()) {
            Ok(()) => {
                self.dirty = false;
                bump_fs_revision();
                self.then(after)
            }
            Err(e) => {
                let mut msg = String::from("Couldn't save: ");
                msg.push_str(e);
                self.prompt = Some(Prompt::Error(msg));
                Reply::repaint(true)
            }
        }
    }

    fn ask_name(&mut self, after: After) -> Reply {
        let suggestion = match &self.path {
            Some(p) => p.clone(),
            None => String::from("/Untitled.txt"),
        };
        let mut field = NameEdit::new(0, &suggestion, false);
        field.allow_slash = true;
        // Select just the name, not its folder or extension.
        let start = suggestion.rfind('/').map_or(0, |i| i + 1);
        let end = suggestion.rfind('.').filter(|&d| d > start).unwrap_or(suggestion.len());
        field.select = Some((start, end));
        field.caret = end;
        self.prompt = Some(Prompt::SaveAs(field, after));
        Reply::repaint(true)
    }

    fn save_as_commit(&mut self, force: bool) -> Reply {
        let (mut name, after) = match &self.prompt {
            Some(Prompt::SaveAs(f, a)) => (String::from(f.text.trim()), a.clone()),
            Some(Prompt::Replace(n, a)) => (n.clone(), a.clone()),
            _ => return Reply::default(),
        };
        if name.is_empty() || name.ends_with('/') {
            return Reply::default();
        }
        if !name.starts_with('/') {
            name.insert(0, '/');
        }
        if !base_name(&name).contains('.') {
            name.push_str(".txt");
        }
        if !matches!(crate::fat16::stat_path(parent_path(&name)), Ok((true, _))) {
            self.prompt = Some(Prompt::Error(String::from("Couldn't save: that folder doesn't exist.")));
            return Reply::repaint(true);
        }
        let taken = crate::fat16::stat_path(&name).is_ok();
        let same = self.path.as_ref().is_some_and(|p| p.eq_ignore_ascii_case(&name));
        if taken && !same && !force {
            self.prompt = Some(Prompt::Replace(name, after));
            return Reply::repaint(true);
        }
        self.path = Some(name);
        self.save(after)
    }

    /// Whatever the user was doing that needs the changes dealt with
    /// first: ask, or just go ahead if there are none.
    fn guard(&mut self, after: After) -> Reply {
        if self.dirty {
            self.prompt = Some(Prompt::Unsaved(after));
            Reply::repaint(true)
        } else {
            self.then(after)
        }
    }

    fn prompt_button(&mut self, i: u8) -> Reply {
        let Some(prompt) = self.prompt.take() else { return Reply::default() };
        match (prompt, i) {
            (p @ Prompt::SaveAs(..), 0) => {
                self.prompt = Some(p);
                self.save_as_commit(false)
            }
            (p @ Prompt::Replace(..), 0) => {
                self.prompt = Some(p);
                self.save_as_commit(true)
            }
            (Prompt::Unsaved(after), 0) => self.save(after),
            (Prompt::Unsaved(after), 1) => {
                self.dirty = false;
                self.then(after)
            }
            _ => Reply::repaint(true),
        }
    }

    fn ctl(&mut self, c: Ctl) -> Reply {
        match c {
            Ctl::New => self.guard(After::New),
            Ctl::Save => self.save(After::Nothing),
            Ctl::SaveAs => self.ask_name(After::Nothing),
            Ctl::Cut => self.edit_key(b'x', MOD_CTRL),
            Ctl::Copy => self.edit_key(b'c', MOD_CTRL),
            Ctl::Paste => self.edit_key(b'v', MOD_CTRL),
            Ctl::PromptBtn(i) => self.prompt_button(i),
            Ctl::Field => Reply::default(),
        }
    }

    /// A key for the page itself.
    fn edit_key(&mut self, code: u8, mods: u8) -> Reply {
        let shift = mods & MOD_SHIFT != 0;
        let ctrl = mods & MOD_CTRL != 0;
        let (l, c) = self.cur;
        let last = self.lines.len() - 1;
        let vis = self.visible_lines();
        let mut vertical = false;
        match code {
            KEY_LEFT => {
                let to = if ctrl {
                    self.word_left(self.cur)
                } else if let (Some((s, _)), false) = (self.selection(), shift) {
                    s
                } else if c > 0 {
                    (l, c - 1)
                } else if l > 0 {
                    (l - 1, self.lines[l - 1].len())
                } else {
                    (0, 0)
                };
                self.move_to(to, shift);
            }
            KEY_RIGHT => {
                let to = if ctrl {
                    self.word_right(self.cur)
                } else if let (Some((_, e)), false) = (self.selection(), shift) {
                    e
                } else if c < self.lines[l].len() {
                    (l, c + 1)
                } else if l < last {
                    (l + 1, 0)
                } else {
                    (l, c)
                };
                self.move_to(to, shift);
            }
            KEY_UP | KEY_DOWN | KEY_PAGE_UP | KEY_PAGE_DOWN => {
                let want = *self.want_col.get_or_insert(c);
                let target = match code {
                    KEY_UP => l.saturating_sub(1),
                    KEY_DOWN => (l + 1).min(last),
                    KEY_PAGE_UP => l.saturating_sub(vis),
                    _ => (l + vis).min(last),
                };
                let col = if target == l && code == KEY_UP { 0 } else if target == l && code == KEY_DOWN { self.lines[l].len() } else { want.min(self.lines[target].len()) };
                if matches!(code, KEY_PAGE_UP | KEY_PAGE_DOWN) {
                    self.top = if code == KEY_PAGE_UP { self.top.saturating_sub(vis) } else { (self.top + vis).min(last) };
                }
                self.move_to((target, col), shift);
                vertical = true;
            }
            KEY_HOME => {
                // First Home goes to the indentation, the next to column 0.
                let indent = self.lines[l].len() - self.lines[l].trim_start().len();
                let to = if ctrl { (0, 0) } else if c == indent { (l, 0) } else { (l, indent) };
                self.move_to(to, shift);
            }
            KEY_END => {
                let to = if ctrl { (last, self.lines[last].len()) } else { (l, self.lines[l].len()) };
                self.move_to(to, shift);
            }
            KEY_BACKSPACE => {
                self.checkpoint(EditKind::Deleting);
                if !self.delete_selection() {
                    if ctrl {
                        self.anchor = Some(self.word_left(self.cur));
                        self.delete_selection();
                    } else if c > 0 {
                        self.lines[l].remove(c - 1);
                        self.cur = (l, c - 1);
                    } else if l > 0 {
                        let line = self.lines.remove(l);
                        let col = self.lines[l - 1].len();
                        self.lines[l - 1].push_str(&line);
                        self.cur = (l - 1, col);
                    }
                }
            }
            KEY_DELETE => {
                self.checkpoint(EditKind::Deleting);
                if !self.delete_selection() {
                    if c < self.lines[l].len() {
                        self.lines[l].remove(c);
                    } else if l < last {
                        let next = self.lines.remove(l + 1);
                        self.lines[l].push_str(&next);
                    }
                }
            }
            KEY_ENTER => {
                self.checkpoint(EditKind::None);
                // Keep the current line's indentation.
                let indent = self.lines[l].len() - self.lines[l].trim_start().len();
                let mut s = String::from("\n");
                for _ in 0..indent.min(c) {
                    s.push(' ');
                }
                self.insert(&s);
            }
            KEY_TAB => {
                self.checkpoint(EditKind::Typing);
                let n = 4 - self.cur.1 % 4;
                self.insert(&"    "[..n]);
            }
            KEY_ESC => self.anchor = None,
            b'a' if ctrl => {
                self.anchor = Some((0, 0));
                self.cur = (last, self.lines[last].len());
            }
            b'c' | b'x' if ctrl => {
                if self.selection().is_some() {
                    set_clipboard(self.selected_text());
                    if code == b'x' {
                        self.checkpoint(EditKind::None);
                        self.delete_selection();
                    }
                }
            }
            b'v' if ctrl => {
                let text = clipboard();
                if !text.is_empty() {
                    self.checkpoint(EditKind::None);
                    self.insert(&text);
                }
            }
            b'z' | b'y' if ctrl => {
                let redo = code == b'y' || shift;
                let (from, to) = if redo { (&mut self.redo, &mut self.undo) } else { (&mut self.undo, &mut self.redo) };
                if let Some((lines, cur)) = from.pop() {
                    to.push((core::mem::replace(&mut self.lines, lines), self.cur));
                    self.cur = cur;
                    self.anchor = None;
                    self.dirty = true;
                }
                self.last_edit = EditKind::None;
            }
            b's' if ctrl => return if shift { self.ask_name(After::Nothing) } else { self.save(After::Nothing) },
            b'n' if ctrl => return self.guard(After::New),
            c if (0x20..0x7f).contains(&c) && mods & (MOD_CTRL | MOD_ALT) == 0 => {
                self.checkpoint(EditKind::Typing);
                let ch = typed_char(c, mods);
                let mut buf = [0u8; 1];
                buf[0] = ch;
                self.insert(core::str::from_utf8(&buf).unwrap_or("?"));
                if ch == b' ' {
                    self.last_edit = EditKind::None; // A new undo step per word.
                }
            }
            _ => return Reply::default(),
        }
        if !vertical {
            self.want_col = None;
        }
        self.cur.0 = self.cur.0.min(self.lines.len() - 1);
        self.cur.1 = self.cur.1.min(self.lines[self.cur.0].len());
        self.reveal();
        Reply::repaint(true)
    }

    fn paint_toolbar(&self, p: &mut Painter, w: i32) {
        for (c, ic) in [(Ctl::New, icon::DOCUMENT_ADD_20), (Ctl::Save, icon::SAVE_20), (Ctl::Cut, icon::CUT_20), (Ctl::Copy, icon::COPY_20), (Ctl::Paste, icon::PASTE_20)] {
            let r = Self::tool_rect(c);
            let on = self.enabled(c);
            if on && self.hover == Some(c) {
                p.fill_squircle(r, 9.0, TEXT, 34);
            }
            let (iw, ih, m) = assets::icon(ic);
            p.draw_mask(r.x + (r.w - iw) / 2, r.y + (r.h - ih) / 2, iw, ih, m, TEXT, if on { 230 } else { 80 });
        }
        let r = Self::tool_rect(Ctl::SaveAs);
        p.fill_squircle(r, 9.0, TEXT, if self.hover == Some(Ctl::SaveAs) { 40 } else { 18 });
        p.text(&font::UI, r.x + 10, r.y + (r.h - font::UI.line_height()) / 2, "Save as...", TEXT, 255);
        p.fill_rect(Rect::new(Self::tool_rect(Ctl::Cut).x - 8, 14, 1, 16), TEXT, 40);

        let mut pos = String::new();
        let _ = write!(pos, "Ln {}, Col {}", self.cur.0 + 1, self.cur.1 + 1);
        if let Some((s, e)) = self.selection() {
            let n = if s.0 == e.0 { e.1 - s.1 } else { self.selected_text().len() };
            let _ = write!(pos, "  ({n} selected)");
        }
        let pw = font::SMALL.width(&pos);
        p.text(&font::SMALL, w - 20 - pw, 15, &pos, TEXT_DIM, 255);
    }

    fn paint_prompt(&self, p: &mut Painter) {
        let Some(prompt) = &self.prompt else { return };
        let r = self.prompt_rect();
        p.fill_squircle(r, 14.0, rgb(10, 12, 16), 200);
        let ty = r.y + (r.h - font::UI.line_height()) / 2;
        let (ic, color) = match prompt {
            Prompt::Error(_) | Prompt::Replace(..) => (icon::WARNING_20, rgb(255, 196, 92)),
            _ => (icon::SAVE_20, accent()),
        };
        let (iw, ih, m) = assets::icon(ic);
        p.draw_mask(r.x + 14, r.y + (r.h - ih) / 2, iw, ih, m, color, 255);
        let mut msg = String::new();
        let name = self.path.as_deref().map_or("Untitled", base_name);
        match prompt {
            Prompt::SaveAs(field, _) => {
                p.text(&font::UI_BOLD, r.x + 44, ty, "Save as", TEXT, 255);
                paint_field(p, self.field_rect(), &field.text, field.caret, field.select);
            }
            Prompt::Unsaved(_) => {
                msg.push_str("Save changes to ");
                msg.push_str(name);
                msg.push('?');
            }
            Prompt::Replace(n, _) => {
                msg.push_str(base_name(n));
                msg.push_str(" already exists. Replace it?");
            }
            Prompt::Error(e) => msg.push_str(e),
        }
        if !msg.is_empty() {
            let right = self.prompt_button_rect(0).x - 8;
            p.clipped(Rect::new(r.x, r.y, right - r.x, r.h)).text(&font::UI, r.x + 44, ty, &msg, TEXT, 255);
        }
        for (i, label) in self.prompt_buttons().iter().enumerate() {
            let b = self.prompt_button_rect(i);
            let hot = self.hover == Some(Ctl::PromptBtn(i as u8));
            if i == 0 {
                p.fill_squircle(b, 10.0, accent(), if hot { 255 } else { 220 });
            } else {
                p.fill_squircle(b, 10.0, TEXT, if hot { 50 } else { 26 });
            }
            let color = if i == 0 { rgb(14, 22, 26) } else { TEXT };
            p.text(&font::UI, b.x + (b.w - font::UI.width(label)) / 2, b.y + (b.h - font::UI.line_height()) / 2, label, color, 255);
        }
    }

    /// The caret's rectangle, in client coordinates (empty if scrolled away).
    fn caret_rect(&self) -> Rect {
        let (cw, lh) = cell();
        if self.cur.0 < self.top || self.cur.0 >= self.top + self.visible_lines() || self.cur.1 < self.left {
            return Rect::default();
        }
        let x = self.text_x() + (self.cur.1 - self.left) as i32 * cw;
        let y = self.text_y() + (self.cur.0 - self.top) as i32 * lh;
        Rect::new(x - 1, y, 3, lh)
    }
}

impl App for Notepad {
    fn client_size(&self) -> (i32, i32) {
        (720, 480)
    }

    fn min_size(&self) -> (i32, i32) {
        (460, 240)
    }

    fn resized(&mut self, w: i32, h: i32) {
        self.w = w;
        self.h = h;
    }

    fn title(&self) -> Option<String> {
        let mut t = String::new();
        if self.dirty {
            t.push('*');
        }
        t.push_str(self.path.as_deref().map_or("Untitled", base_name));
        t.push_str(" - Notepad");
        Some(t)
    }

    fn tick(&mut self) -> Option<Rect> {
        let now = crate::timer::ticks();
        if now.wrapping_sub(self.blink_at) < BLINK {
            return None;
        }
        self.blink_at = now;
        self.blink_on = !self.blink_on;
        let r = self.caret_rect();
        (!r.is_empty()).then_some(r)
    }

    fn paint(&mut self, p: &mut Painter, w: i32, h: i32) {
        self.w = w;
        self.h = h;
        self.paint_toolbar(p, w);
        self.paint_prompt(p);

        let page = self.page();
        p.fill_squircle(page, 18.0, rgb(4, 8, 12), 92);
        let gutter = Rect::new(page.x, page.y, self.gutter_w(), page.h);
        p.fill_rect(Rect::new(gutter.right(), page.y + 10, 1, page.h - 20), TEXT, 22);

        let (cw, lh) = cell();
        let (tx, ty) = (self.text_x(), self.text_y());
        let vis = self.visible_lines();
        let cols = self.visible_cols() + 1;
        let sel = self.selection();
        let mut num = String::new();
        let mut tp = p.clipped(Rect::new(page.x, page.y + 4, page.w - 6, page.h - 8));
        for row in 0..vis {
            let l = self.top + row;
            if l >= self.lines.len() {
                break;
            }
            let y = ty + row as i32 * lh;
            num.clear();
            let _ = write!(num, "{}", l + 1);
            let current = l == self.cur.0;
            tp.text(&font::MONO, gutter.right() - 10 - font::MONO.width(&num), y + 1, &num, if current { TEXT } else { TEXT_DIM }, if current { 230 } else { 120 });
            let line = self.lines[l].as_bytes();
            if let Some((s, e)) = sel.filter(|(s, e)| l >= s.0 && l <= e.0) {
                let a = if l == s.0 { s.1 } else { 0 };
                // A selected line break shows as a little extra width.
                let b = if l == e.0 { e.1 } else { line.len() + 1 };
                let (a, b) = (a.max(self.left), b.min(self.left + cols));
                if b > a {
                    tp.fill_rect(Rect::new(tx + (a - self.left) as i32 * cw, y, (b - a) as i32 * cw, lh), accent(), 90);
                }
            }
            for (k, &ch) in line.iter().enumerate().skip(self.left).take(cols) {
                if ch != b' ' {
                    let buf = [ch];
                    tp.text(&font::MONO, tx + (k - self.left) as i32 * cw, y + 1, core::str::from_utf8(&buf).unwrap_or("?"), TEXT, 245);
                }
            }
        }
        let typing_name = matches!(self.prompt, Some(Prompt::SaveAs(..)));
        if self.blink_on && !typing_name {
            let c = self.caret_rect();
            if !c.is_empty() {
                tp.fill_rect(Rect::new(c.x + 1, c.y + 1, 2, c.h - 2), accent(), 255);
            }
        }

        // Scroll position, when the text is longer than the page.
        let n = self.lines.len();
        if n > vis {
            let track = page.h - 24;
            let th = ((track as usize * vis / n) as i32).max(24);
            let span = (n - vis) as i32;
            let y = page.y + 12 + (track - th) * (self.top.min(n - vis) as i32) / span.max(1);
            p.fill_squircle(Rect::new(page.right() - 9, y, 4, th), 2.0, TEXT, 90);
        }
    }

    fn mouse(&mut self, ev: MouseEvent, x: i32, y: i32, _w: i32, _h: i32) -> Reply {
        match ev {
            MouseEvent::Move => {
                let h = self.ctl_at(x, y);
                let changed = h != self.hover;
                self.hover = h;
                Reply::repaint(changed)
            }
            MouseEvent::Down | MouseEvent::DoubleClick => {
                if let Some(c) = self.ctl_at(x, y) {
                    let reply = self.ctl(c);
                    self.hover = self.ctl_at(x, y);
                    return reply;
                }
                if !self.page().contains(x, y) || matches!(self.prompt, Some(Prompt::Unsaved(_) | Prompt::Replace(..))) {
                    return Reply::default();
                }
                if matches!(self.prompt, Some(Prompt::SaveAs(..) | Prompt::Error(_))) {
                    self.prompt = None; // Clicking back into the page cancels it.
                }
                let pos = self.pos_at(x, y);
                if ev == MouseEvent::DoubleClick {
                    // Select the word under the pointer.
                    let b = self.lines[pos.0].as_bytes();
                    let (mut a, mut e) = (pos.1, pos.1);
                    while a > 0 && is_word(b[a - 1]) {
                        a -= 1;
                    }
                    while e < b.len() && is_word(b[e]) {
                        e += 1;
                    }
                    self.anchor = Some((pos.0, a));
                    self.cur = (pos.0, e);
                } else {
                    self.cur = pos;
                    self.anchor = Some(pos);
                    self.dragging = true;
                }
                self.want_col = None;
                self.last_edit = EditKind::None;
                self.reveal();
                Reply::repaint(true)
            }
            MouseEvent::Drag if self.dragging => {
                // Dragging past the top or bottom scrolls.
                let page = self.page();
                if y < page.y && self.top > 0 {
                    self.top -= 1;
                } else if y > page.bottom() && self.top + self.visible_lines() < self.lines.len() {
                    self.top += 1;
                }
                let pos = self.pos_at(x, y.clamp(page.y + 10, page.bottom() - 12));
                if pos == self.cur {
                    return Reply::default();
                }
                self.cur = pos;
                self.reveal();
                Reply::repaint(true)
            }
            MouseEvent::Up => {
                self.dragging = false;
                if self.anchor == Some(self.cur) {
                    self.anchor = None;
                }
                Reply::repaint(true)
            }
            _ => Reply::default(),
        }
    }

    fn wheel(&mut self, notches: i32, _x: i32, _y: i32, _w: i32, _h: i32) -> Reply {
        let max = self.lines.len().saturating_sub(self.visible_lines()) as i64;
        let top = (self.top as i64 + notches as i64 * 3).clamp(0, max.max(0)) as usize;
        let changed = top != self.top;
        self.top = top;
        Reply::repaint(changed)
    }

    fn cursor(&self, x: i32, y: i32, _w: i32, _h: i32) -> Cursor {
        match self.ctl_at(x, y) {
            Some(Ctl::Field) => Cursor::Text,
            Some(_) => Cursor::Hand,
            None if self.page().contains(x, y) && x >= self.text_x() - 4 => Cursor::Text,
            None => Cursor::Arrow,
        }
    }

    fn context_menu(&mut self, x: i32, y: i32, _w: i32, _h: i32) -> Vec<super::apps::ContextItem> {
        if !self.page().contains(x, y) {
            return Vec::new();
        }
        // Right-clicking outside the selection moves the caret there.
        let pos = self.pos_at(x, y);
        let inside = self.selection().is_some_and(|(s, e)| pos >= s && pos <= e);
        if !inside {
            self.cur = pos;
            self.anchor = None;
        }
        let sel = self.selection().is_some();
        alloc::vec![
            ("Undo", (!self.undo.is_empty()).then_some(0)),
            ("", None),
            ("Cut", sel.then_some(1)),
            ("Copy", sel.then_some(2)),
            ("Paste", (!clipboard().is_empty()).then_some(3)),
            ("", None),
            ("Select All", Some(4)),
        ]
    }

    fn context_cmd(&mut self, id: u32) -> Reply {
        let key = [b'z', b'x', b'c', b'v', b'a'][id.min(4) as usize];
        self.edit_key(key, MOD_CTRL)
    }

    fn wants_keys(&self) -> bool {
        true
    }

    fn key(&mut self, code: u8, mods: u8) -> Reply {
        match &mut self.prompt {
            Some(Prompt::SaveAs(field, _)) => match code {
                KEY_ENTER => self.save_as_commit(false),
                KEY_ESC => {
                    self.prompt = None;
                    Reply::repaint(true)
                }
                _ => {
                    field.key(code, mods);
                    Reply::repaint(true)
                }
            },
            Some(Prompt::Unsaved(_) | Prompt::Replace(..) | Prompt::Error(_)) => match code {
                KEY_ENTER => self.prompt_button(0),
                KEY_ESC => {
                    self.prompt = None;
                    Reply::repaint(true)
                }
                _ => Reply::default(),
            },
            None => self.edit_key(code, mods),
        }
    }

    fn request_close(&mut self) -> bool {
        if self.dirty {
            self.prompt = Some(Prompt::Unsaved(After::Close));
            false
        } else {
            true
        }
    }

    fn open_path(&mut self, path: &str) {
        if self.path.as_ref().is_some_and(|p| p.eq_ignore_ascii_case(path)) {
            return;
        }
        let _ = self.guard(After::Open(String::from(path)));
    }
}
