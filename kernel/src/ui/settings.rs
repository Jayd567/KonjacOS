//! Settings: the desktop's preferences and the app that edits them.
//!
//! The current [`Settings`] live in one global, bumped to a new
//! [`revision`] on every change; the desktop watches that, applies what
//! changed (a new wallpaper, recomputed glass, a reformatted clock) and
//! saves everything to `/DESKTOP.CFG` as `set <key> <value>` lines next
//! to the pins and shortcuts (see `icons.rs`).
//!
//! The app is split into sections (Personalization, Mouse, Date & time,
//! Storage, System) picked from a sidebar, with a search box above them
//! that finds a setting by name or by related words ("blur", "ram").

extern crate alloc;

use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::apps::{App, MouseEvent, Reply, TEXT, TEXT_DIM};
use super::assets;
use super::font;
use super::icon_ids as icon;
use super::surface::{rgb, Painter, Rect};
use super::sysmon;
use crate::cursor::Shape as Cursor;
use crate::sync::SpinLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Settings {
    /// Index into [`WALLPAPERS`].
    pub wallpaper: u8,
    /// Index into [`ACCENTS`].
    pub accent: u8,
    /// 0 = clear, 1 = balanced, 2 = frosted.
    pub glass: u8,
    /// 0 = slow, 1 = normal, 2 = fast.
    pub pointer: u8,
    /// 0 = slow, 1 = normal, 2 = fast.
    pub double_click: u8,
    pub clock_24h: bool,
}

const DEFAULT: Settings = Settings { wallpaper: 0, accent: 0, glass: 1, pointer: 1, double_click: 1, clock_24h: false };

pub const WALLPAPERS: [&str; 4] = ["Photo", "Aurora", "Dusk", "Graphite"];
pub const ACCENTS: [(&str, u32); 6] = [
    ("Mint", rgb(120, 214, 196)),
    ("Sky", rgb(110, 176, 255)),
    ("Lilac", rgb(186, 150, 255)),
    ("Rose", rgb(255, 128, 176)),
    ("Amber", rgb(255, 184, 92)),
    ("Lime", rgb(160, 222, 100)),
];

static SETTINGS: SpinLock<Settings> = SpinLock::new(DEFAULT);
static REVISION: AtomicU64 = AtomicU64::new(0);
static ACCENT: AtomicU32 = AtomicU32::new(ACCENTS[0].1);

pub fn get() -> Settings {
    *SETTINGS.lock()
}

/// Changes the settings. Things the kernel reads directly (the accent,
/// glass strength, pointer speed) take effect at once; the desktop picks
/// up the rest when it sees the new revision.
pub fn set(s: Settings) {
    *SETTINGS.lock() = s;
    ACCENT.store(ACCENTS[s.accent as usize % ACCENTS.len()].1, Ordering::Relaxed);
    super::glass::set_level(s.glass);
    crate::mouse::set_speed_percent([60, 100, 170][s.pointer.min(2) as usize]);
    REVISION.fetch_add(1, Ordering::Release);
}

pub fn revision() -> u64 {
    REVISION.load(Ordering::Acquire)
}

/// The accent colour: selection highlights, Start tiles, links.
pub fn accent() -> u32 {
    ACCENT.load(Ordering::Relaxed)
}

/// Ticks between two clicks for them to count as a double-click.
pub fn double_click_ticks() -> u64 {
    [60, 40, 25][get().double_click.min(2) as usize]
}

/// Applies one `set <key> <value>` line from `/DESKTOP.CFG` to `s`.
pub fn parse_line(s: &mut Settings, key: &str, value: &str) {
    let Ok(v) = value.parse::<u8>() else { return };
    match key {
        "wallpaper" => s.wallpaper = v.min(WALLPAPERS.len() as u8 - 1),
        "accent" => s.accent = v.min(ACCENTS.len() as u8 - 1),
        "glass" => s.glass = v.min(2),
        "pointer" => s.pointer = v.min(2),
        "double_click" => s.double_click = v.min(2),
        "clock_24h" => s.clock_24h = v != 0,
        _ => {}
    }
}

/// The `set` lines for `/DESKTOP.CFG`.
pub fn write_lines(out: &mut alloc::string::String) {
    let s = get();
    let _ = writeln!(out, "set wallpaper {}", s.wallpaper);
    let _ = writeln!(out, "set accent {}", s.accent);
    let _ = writeln!(out, "set glass {}", s.glass);
    let _ = writeln!(out, "set pointer {}", s.pointer);
    let _ = writeln!(out, "set double_click {}", s.double_click);
    let _ = writeln!(out, "set clock_24h {}", s.clock_24h as u8);
}

// --- Wallpapers -----------------------------------------------------------------

/// A soft colour blob: centre and radius as fractions of the screen
/// (radius of the width), and its colour.
struct Blob(i32, i32, i32, (i32, i32, i32));

/// The drawn wallpapers: a base colour with large soft blobs of light,
/// positions in 1/1000 of the screen.
fn blobs(index: u8) -> ((i32, i32, i32), [Blob; 3]) {
    match index {
        1 => ((6, 18, 30), [Blob(200, 300, 560, (40, 196, 168)), Blob(820, 220, 500, (118, 64, 204)), Blob(600, 950, 620, (28, 92, 206))]),
        2 => ((28, 12, 40), [Blob(220, 850, 620, (250, 132, 64)), Blob(760, 620, 560, (222, 72, 142)), Blob(500, 80, 600, (88, 52, 172))]),
        _ => ((22, 24, 28), [Blob(300, 280, 700, (78, 84, 96)), Blob(820, 820, 600, (44, 48, 58)), Blob(700, 180, 420, (112, 116, 124))]),
    }
}

/// Wallpaper `index` (into [`WALLPAPERS`]) at `w x h`, as `0x00RRGGBB`.
pub fn wallpaper(index: u8, w: i32, h: i32) -> Vec<u32> {
    if index == 0 {
        return assets::wallpaper(w, h);
    }
    let (base, blobs) = blobs(index);
    let mut out = alloc::vec![0u32; (w * h).max(0) as usize];
    // Each blob in pixels: centre, and 1/r^2 in 24.8 fixed point.
    let px: Vec<(i64, i64, i64, (i32, i32, i32))> = blobs
        .iter()
        .map(|b| {
            let r = (b.2 as i64 * w as i64 / 1000).max(1);
            (b.0 as i64 * w as i64 / 1000, b.1 as i64 * h as i64 / 1000, (1i64 << 32) / (r * r), b.3)
        })
        .collect();
    for y in 0..h {
        for x in 0..w {
            let (mut r, mut g, mut b) = base;
            for &(cx, cy, inv_r2, (br, bg, bb)) in &px {
                let (dx, dy) = (x as i64 - cx, y as i64 - cy);
                // t = d^2 / r^2 in 1/256 units; weight = (1 - t)^2.
                let t = ((dx * dx + dy * dy) * inv_r2) >> 24;
                if t < 256 {
                    let k = ((256 - t) * (256 - t)) as i32 >> 8;
                    r += ((br - r) * k) >> 8;
                    g += ((bg - g) * k) >> 8;
                    b += ((bb - b) * k) >> 8;
                }
            }
            out[(y * w + x) as usize] = ((r.clamp(0, 255) as u32) << 16) | ((g.clamp(0, 255) as u32) << 8) | b.clamp(0, 255) as u32;
        }
    }
    out
}

// --- The Settings app -------------------------------------------------------------

/// The sidebar: a search box, then one entry per section.
const SIDEBAR_W: i32 = 212;
/// Left edge of the section content.
const CX: i32 = SIDEBAR_W + 28;
const THUMB_W: i32 = 112;
const THUMB_H: i32 = 70;
const SEG_W: i32 = 124;
const SEG_H: i32 = 34;
const NAV_TOP: i32 = 62;
const NAV_H: i32 = 40;
const RESULT_H: i32 = 46;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
    Personalization,
    Mouse,
    DateTime,
    Storage,
    System,
}

const SECTIONS: [Section; 5] = [Section::Personalization, Section::Mouse, Section::DateTime, Section::Storage, Section::System];

impl Section {
    fn name(self) -> &'static str {
        match self {
            Section::Personalization => "Personalization",
            Section::Mouse => "Mouse",
            Section::DateTime => "Date & time",
            Section::Storage => "Storage",
            Section::System => "System",
        }
    }

    fn icon(self) -> usize {
        match self {
            Section::Personalization => icon::COLOR_20,
            Section::Mouse => icon::CURSOR_20,
            Section::DateTime => icon::CLOCK_20,
            Section::Storage => icon::STORAGE_20,
            Section::System => icon::LAPTOP_20,
        }
    }
}

/// What search looks through: each setting's name, extra words people
/// might type for it, and where it lives.
const INDEX: [(&str, &str, Section); 10] = [
    ("Wallpaper", "background image photo picture aurora dusk graphite desktop", Section::Personalization),
    ("Accent colour", "color theme highlight", Section::Personalization),
    ("Glass", "transparency transparent blur frosted clear effects", Section::Personalization),
    ("Pointer speed", "mouse cursor sensitivity fast slow", Section::Mouse),
    ("Double-click speed", "mouse click", Section::Mouse),
    ("Clock format", "time 24-hour 12-hour am pm", Section::DateTime),
    ("Date and time", "clock calendar today", Section::DateTime),
    ("Disk space", "storage drive free used fat16", Section::Storage),
    ("Memory", "ram system usage", Section::System),
    ("About this computer", "version kernel system uptime info", Section::System),
];

pub fn contains_ignore_case(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    n.is_empty() || h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Settings matching `query`, for the Start menu's search: the setting's
/// name, its section's name (what [`App::navigate`] takes) and icon.
pub fn search(query: &str) -> Vec<(&'static str, &'static str, usize)> {
    INDEX
        .iter()
        .filter(|(name, words, sec)| contains_ignore_case(name, query) || contains_ignore_case(words, query) || contains_ignore_case(sec.name(), query))
        .map(|&(name, _, sec)| (name, sec.name(), sec.icon()))
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Control {
    Search,
    Nav(Section),
    Result(usize),
    Wallpaper(u8),
    Accent(u8),
    Glass(u8),
    Pointer(u8),
    DoubleClick(u8),
    Clock(u8),
}

pub struct SettingsApp {
    thumbs: Vec<Vec<u32>>,
    section: Section,
    hover: Option<Control>,
    query: alloc::string::String,
    /// The search box has keyboard focus.
    typing: bool,
    /// `(used, total)` bytes on the `/` disk and its format, read when
    /// Storage opens.
    disk: Option<(u64, u64, &'static str)>,
    snap: sysmon::Snapshot,
    seq: u64,
}

impl SettingsApp {
    pub fn new() -> Self {
        let thumbs = (0..WALLPAPERS.len() as u8).map(|i| wallpaper(i, THUMB_W, THUMB_H)).collect();
        SettingsApp {
            thumbs,
            section: Section::Personalization,
            hover: None,
            query: alloc::string::String::new(),
            typing: false,
            disk: None,
            snap: sysmon::latest(),
            seq: u64::MAX,
        }
    }

    fn search_rect() -> Rect {
        Rect::new(14, 12, SIDEBAR_W - 22, 36)
    }

    fn nav_rect(i: usize) -> Rect {
        Rect::new(10, NAV_TOP + i as i32 * NAV_H, SIDEBAR_W - 14, NAV_H - 4)
    }

    fn results(&self) -> Vec<usize> {
        let q = self.query.trim();
        (0..INDEX.len())
            .filter(|&i| {
                let (name, words, sec) = INDEX[i];
                contains_ignore_case(name, q) || contains_ignore_case(words, q) || contains_ignore_case(sec.name(), q)
            })
            .collect()
    }

    fn searching(&self) -> bool {
        !self.query.trim().is_empty()
    }

    fn open(&mut self, section: Section) {
        self.section = section;
        self.query.clear();
        self.typing = false;
        if section == Section::Storage {
            self.disk = crate::vfs::volume_stats()
                .ok()
                .map(|v| ((v.total_blocks - v.free_blocks) * v.block_bytes, v.total_blocks * v.block_bytes, v.name));
        }
    }

    /// Row `row`'s heading y, and its segmented control's rectangle for
    /// option `i`, in a section made of headed rows.
    fn segment_rect(top: i32, i: u8) -> Rect {
        Rect::new(CX + i as i32 * (SEG_W + 4), top + 24, SEG_W, SEG_H)
    }

    fn thumb_rect(i: u8) -> Rect {
        Rect::new(CX + i as i32 * (THUMB_W + 14), 90, THUMB_W, THUMB_H)
    }

    fn accent_rect(i: u8) -> Rect {
        Rect::new(CX + 2 + i as i32 * 46, 230, 32, 32)
    }

    /// Every clickable thing on screen right now, with where it is.
    fn controls(&self) -> Vec<(Control, Rect)> {
        let mut v = alloc::vec![(Control::Search, Self::search_rect())];
        for (i, &s) in SECTIONS.iter().enumerate() {
            v.push((Control::Nav(s), Self::nav_rect(i)));
        }
        if self.searching() {
            for (n, r) in self.results().into_iter().enumerate() {
                v.push((Control::Result(r), Rect::new(CX - 8, 74 + n as i32 * RESULT_H, 500, RESULT_H - 6)));
            }
            return v;
        }
        let seg = |v: &mut Vec<(Control, Rect)>, top: i32, n: u8, make: fn(u8) -> Control| {
            for i in 0..n {
                v.push((make(i), Self::segment_rect(top, i)));
            }
        };
        match self.section {
            Section::Personalization => {
                for i in 0..WALLPAPERS.len() as u8 {
                    v.push((Control::Wallpaper(i), Self::thumb_rect(i)));
                }
                for i in 0..ACCENTS.len() as u8 {
                    v.push((Control::Accent(i), Self::accent_rect(i)));
                }
                seg(&mut v, 290, 3, Control::Glass);
            }
            Section::Mouse => {
                seg(&mut v, 66, 3, Control::Pointer);
                seg(&mut v, 150, 3, Control::DoubleClick);
            }
            Section::DateTime => seg(&mut v, 160, 2, Control::Clock),
            Section::Storage | Section::System => {}
        }
        v
    }

    fn control_at(&self, x: i32, y: i32) -> Option<Control> {
        self.controls().into_iter().find(|(_, r)| r.contains(x, y)).map(|(c, _)| c)
    }

    fn selected(s: &Settings, c: Control) -> bool {
        match c {
            Control::Wallpaper(i) => s.wallpaper == i,
            Control::Accent(i) => s.accent == i,
            Control::Glass(i) => s.glass == i,
            Control::Pointer(i) => s.pointer == i,
            Control::DoubleClick(i) => s.double_click == i,
            Control::Clock(i) => s.clock_24h == (i == 1),
            _ => false,
        }
    }

    fn paint_segments(&self, p: &mut Painter, s: &Settings, top: i32, title: &str, options: &[&str], make: fn(u8) -> Control) {
        heading(p, top, title);
        let n = options.len() as u8;
        let track = Self::segment_rect(top, 0).union(&Self::segment_rect(top, n - 1)).expand(3);
        p.fill_squircle(track, 13.0, rgb(0, 0, 0), 70);
        for (i, label) in options.iter().enumerate() {
            let c = make(i as u8);
            let r = Self::segment_rect(top, i as u8);
            let on = Self::selected(s, c);
            if on {
                p.fill_squircle(r, 10.0, accent(), 235);
            } else if self.hover == Some(c) {
                p.fill_squircle(r, 10.0, TEXT, 26);
            }
            let color = if on { rgb(14, 22, 26) } else { TEXT };
            p.text(&font::UI, r.x + (r.w - font::UI.width(label)) / 2, r.y + (SEG_H - font::UI.line_height()) / 2, label, color, 255);
        }
    }

    fn paint_sidebar(&self, p: &mut Painter, h: i32) {
        p.fill_squircle(Rect::new(4, 4, SIDEBAR_W - 4, h - 14), 22.0, rgb(0, 0, 0), 46);
        let sr = Self::search_rect();
        p.fill_squircle(sr, 12.0, rgb(0, 0, 0), if self.typing { 110 } else { 70 });
        if self.typing {
            // A thin accent underline marks the focused field.
            p.fill_rect(Rect::new(sr.x + 12, sr.bottom() - 2, sr.w - 24, 2), accent(), 255);
        }
        let (iw, ih, m) = assets::icon(icon::SEARCH_20);
        p.draw_mask(sr.x + 10, sr.y + (sr.h - ih) / 2, iw, ih, m, TEXT_DIM, 255);
        let ty = sr.y + (sr.h - font::UI.line_height()) / 2;
        let tx = sr.x + 38;
        if self.query.is_empty() && !self.typing {
            p.text(&font::UI, tx, ty, "Find a setting", TEXT_DIM, 200);
        } else {
            // Show the end of a long query.
            let max = sr.w - 50;
            let mut start = 0;
            while font::UI.width(&self.query[start..]) > max {
                start += 1;
            }
            let end = tx + p.text(&font::UI, tx, ty, &self.query[start..], TEXT, 255);
            if self.typing {
                p.fill_rect(Rect::new(end + 1, sr.y + 9, 2, sr.h - 18), accent(), 255);
            }
        }

        for (i, &s) in SECTIONS.iter().enumerate() {
            let r = Self::nav_rect(i);
            let current = s == self.section && !self.searching();
            if current {
                p.fill_squircle(r, 11.0, TEXT, 34);
                p.fill_squircle(Rect::new(r.x + 4, r.y + 10, 3, r.h - 20), 1.5, accent(), 255);
            } else if self.hover == Some(Control::Nav(s)) {
                p.fill_squircle(r, 11.0, TEXT, 20);
            }
            let (iw, ih, m) = assets::icon(s.icon());
            p.draw_mask(r.x + 16, r.y + (r.h - ih) / 2, iw, ih, m, if current { accent() } else { TEXT }, 255);
            p.text(&font::UI, r.x + 46, r.y + (r.h - font::UI.line_height()) / 2, s.name(), TEXT, 255);
        }
    }

    fn paint_results(&self, p: &mut Painter) {
        let mut title = alloc::string::String::from("Results for \"");
        title.push_str(self.query.trim());
        title.push('"');
        p.text_shadowed(&font::UI_BOLD, CX, 30, &title, TEXT, 255);
        let results = self.results();
        if results.is_empty() {
            p.text(&font::UI, CX, 80, "No settings match. Try another word.", TEXT_DIM, 255);
            return;
        }
        for (n, &i) in results.iter().enumerate() {
            let r = Rect::new(CX - 8, 74 + n as i32 * RESULT_H, 500, RESULT_H - 6);
            if self.hover == Some(Control::Result(i)) {
                p.fill_squircle(r, 11.0, TEXT, 24);
            }
            let (name, _, sec) = INDEX[i];
            let (iw, ih, m) = assets::icon(sec.icon());
            p.draw_mask(r.x + 12, r.y + (r.h - ih) / 2, iw, ih, m, accent(), 255);
            p.text(&font::UI, r.x + 44, r.y + 4, name, TEXT, 255);
            p.text(&font::SMALL, r.x + 44, r.y + 22, sec.name(), TEXT_DIM, 255);
        }
    }

    /// A card of `label: value` rows.
    fn paint_card(p: &mut Painter, top: i32, w: i32, rows: &[(&str, &str)]) {
        let card = Rect::new(CX - 8, top, w - CX - 16, 36 * rows.len() as i32 + 14);
        p.fill_squircle(card, 16.0, rgb(0, 0, 0), 60);
        for (i, (k, v)) in rows.iter().enumerate() {
            let y = card.y + 14 + i as i32 * 36;
            p.text(&font::UI, card.x + 18, y, k, TEXT_DIM, 255);
            p.text(&font::UI, card.right() - 18 - font::UI.width(v), y, v, TEXT, 255);
            if i + 1 < rows.len() {
                p.fill_rect(Rect::new(card.x + 18, y + 27, card.w - 36, 1), TEXT, 24);
            }
        }
    }

    fn paint_section(&self, p: &mut Painter, w: i32) {
        let s = get();
        p.text_shadowed(&font::DISPLAY, CX, 14, self.section.name(), TEXT, 255);
        let mib = 1024 * 1024;
        match self.section {
            Section::Personalization => {
                heading(p, 66, "WALLPAPER");
                for i in 0..WALLPAPERS.len() as u8 {
                    let r = Self::thumb_rect(i);
                    if s.wallpaper == i {
                        p.fill_squircle(r.expand(4), 14.0, accent(), 255);
                    } else if self.hover == Some(Control::Wallpaper(i)) {
                        p.fill_squircle(r.expand(4), 14.0, TEXT, 70);
                    }
                    p.blit(r.x, r.y, r.w, r.h, &self.thumbs[i as usize], THUMB_W);
                    let name = WALLPAPERS[i as usize];
                    p.text(&font::SMALL, r.x + (r.w - font::SMALL.width(name)) / 2, r.bottom() + 10, name, TEXT, 255);
                }
                heading(p, 206, "ACCENT COLOUR");
                for (i, &(_, c)) in ACCENTS.iter().enumerate() {
                    let r = Self::accent_rect(i as u8);
                    if s.accent == i as u8 {
                        p.fill_squircle(r.expand(4), 14.0, TEXT, 255);
                    } else if self.hover == Some(Control::Accent(i as u8)) {
                        p.fill_squircle(r.expand(4), 14.0, TEXT, 90);
                    }
                    p.fill_squircle(r, 11.0, c, 255);
                }
                self.paint_segments(p, &s, 290, "GLASS", &["Clear", "Balanced", "Frosted"], Control::Glass);
            }
            Section::Mouse => {
                self.paint_segments(p, &s, 66, "POINTER SPEED", &["Slow", "Normal", "Fast"], Control::Pointer);
                self.paint_segments(p, &s, 150, "DOUBLE-CLICK SPEED", &["Slow", "Normal", "Fast"], Control::DoubleClick);
                p.text(&font::SMALL, CX, 222, "How quickly two clicks must follow each other to open something.", TEXT_DIM, 255);
            }
            Section::DateTime => {
                let t = self.snap.time;
                let mut big = alloc::string::String::new();
                let _ = if s.clock_24h {
                    write!(big, "{}:{:02}", t.hour, t.minute)
                } else {
                    let h12 = if t.hour % 12 == 0 { 12 } else { t.hour % 12 };
                    write!(big, "{}:{:02} {}", h12, t.minute, if t.hour < 12 { "AM" } else { "PM" })
                };
                p.text_shadowed(&font::DISPLAY, CX, 66, &big, TEXT, 255);
                let mut date = alloc::string::String::new();
                let _ = write!(date, "{}/{}/{}", t.month, t.day, t.year);
                p.text(&font::UI, CX, 108, &date, TEXT_DIM, 255);
                self.paint_segments(p, &s, 160, "CLOCK FORMAT", &["12-hour", "24-hour"], Control::Clock);
                p.text(&font::SMALL, CX, 232, "The time comes from the computer's clock (CMOS RTC).", TEXT_DIM, 255);
            }
            Section::Storage => {
                heading(p, 66, "DISK");
                let card = Rect::new(CX - 8, 90, w - CX - 16, 112);
                p.fill_squircle(card, 16.0, rgb(0, 0, 0), 60);
                let (iw, ih, m) = assets::icon(icon::HARD_DRIVE_20);
                p.draw_mask(card.x + 18, card.y + 18, iw, ih, m, accent(), 255);
                let mut title = alloc::string::String::from("Data disk");
                if let Some((_, _, name)) = self.disk {
                    title.push_str(" (");
                    title.push_str(name);
                    title.push(')');
                }
                p.text(&font::UI_BOLD, card.x + 48, card.y + 18, &title, TEXT, 255);
                match self.disk {
                    Some((used, total, _)) if total > 0 => {
                        let bar = Rect::new(card.x + 18, card.y + 54, card.w - 36, 10);
                        p.fill_squircle(bar, 5.0, TEXT, 40);
                        let fill = (bar.w as u64 * used / total) as i32;
                        if fill > 0 {
                            p.fill_squircle(Rect::new(bar.x, bar.y, fill.max(10), bar.h), 5.0, accent(), 235);
                        }
                        let mut line = alloc::string::String::new();
                        let _ = write!(line, "{} MB used of {} MB", used / mib, total / mib);
                        p.text(&font::UI, card.x + 18, card.y + 76, &line, TEXT, 255);
                        line.clear();
                        let _ = write!(line, "{} MB free", (total - used) / mib);
                        p.text(&font::UI, card.right() - 18 - font::UI.width(&line), card.y + 76, &line, TEXT_DIM, 255);
                    }
                    _ => {
                        p.text(&font::UI, card.x + 18, card.y + 60, "No data disk attached.", TEXT_DIM, 255);
                    }
                }
                p.text(&font::SMALL, CX, 220, "Desktop shortcuts, pins and these settings are kept in DESKTOP.CFG on this disk.", TEXT_DIM, 255);
            }
            Section::System => {
                let sn = &self.snap;
                let (mut mem, mut up, mut cpu) = (alloc::string::String::new(), alloc::string::String::new(), alloc::string::String::new());
                let _ = write!(mem, "{} of {} MiB in use", sn.mem_used / mib, sn.mem_total / mib);
                let _ = write!(up, "{}h {:02}m {:02}s", sn.uptime_secs / 3600, sn.uptime_secs / 60 % 60, sn.uptime_secs % 60);
                let _ = write!(cpu, "{}%", sn.cpu);
                heading(p, 66, "ABOUT THIS COMPUTER");
                Self::paint_card(p, 90, w, &[("Operating system", concat!("KonjacOS ", env!("CARGO_PKG_VERSION"))), ("Kernel", "x86_64, written in Rust"), ("Boots with", "Limine (BIOS + UEFI)")]);
                heading(p, 228, "RESOURCES");
                Self::paint_card(p, 252, w, &[("Memory", &mem), ("CPU usage", &cpu), ("Uptime", &up)]);
            }
        }
    }
}

fn heading(p: &mut Painter, y: i32, text: &str) {
    p.text(&font::SMALL, CX, y, text, TEXT_DIM, 230);
}

impl App for SettingsApp {
    fn client_size(&self) -> (i32, i32) {
        (SIDEBAR_W + 28 + THUMB_W * 4 + 14 * 3 + 30, 470)
    }

    fn resizable(&self) -> bool {
        false
    }

    fn tick(&mut self) -> Option<Rect> {
        // The clock and system figures stay live while they're showing.
        let seq = sysmon::SEQ.load(Ordering::Acquire);
        if seq == self.seq {
            return None;
        }
        self.seq = seq;
        self.snap = sysmon::latest();
        let live = !self.searching() && matches!(self.section, Section::DateTime | Section::System);
        live.then(|| Rect::new(SIDEBAR_W, 0, i32::MAX / 4, i32::MAX / 4))
    }

    fn paint(&mut self, p: &mut Painter, w: i32, h: i32) {
        self.paint_sidebar(p, h);
        if self.searching() {
            self.paint_results(p);
        } else {
            self.paint_section(p, w);
        }
    }

    fn mouse(&mut self, ev: MouseEvent, x: i32, y: i32, _w: i32, _h: i32) -> Reply {
        let over = self.control_at(x, y);
        match ev {
            MouseEvent::Move => {
                let changed = over != self.hover;
                self.hover = over;
                Reply::repaint(changed)
            }
            MouseEvent::Down | MouseEvent::DoubleClick => {
                self.typing = over == Some(Control::Search);
                let mut s = get();
                match over {
                    Some(Control::Nav(sec)) => self.open(sec),
                    Some(Control::Result(i)) => self.open(INDEX[i].2),
                    Some(Control::Wallpaper(i)) => s.wallpaper = i,
                    Some(Control::Accent(i)) => s.accent = i,
                    Some(Control::Glass(i)) => s.glass = i,
                    Some(Control::Pointer(i)) => s.pointer = i,
                    Some(Control::DoubleClick(i)) => s.double_click = i,
                    Some(Control::Clock(i)) => s.clock_24h = i == 1,
                    Some(Control::Search) | None => {}
                }
                if s != get() {
                    set(s);
                }
                self.hover = self.control_at(x, y);
                Reply::repaint(true)
            }
            _ => Reply::default(),
        }
    }

    fn wants_keys(&self) -> bool {
        self.typing
    }

    fn key(&mut self, code: u8, _mods: u8) -> Reply {
        match code {
            crate::keyboard::KEY_ESC => {
                if self.query.is_empty() {
                    self.typing = false;
                }
                self.query.clear();
            }
            crate::keyboard::KEY_BACKSPACE => {
                self.query.pop();
            }
            crate::keyboard::KEY_ENTER => {
                if let Some(&i) = self.results().first().filter(|_| self.searching()) {
                    self.open(INDEX[i].2);
                }
            }
            c if (c.is_ascii_alphanumeric() || c == b' ' || c == b'-') && self.query.len() < 32 => self.query.push(c as char),
            _ => return Reply::default(),
        }
        self.hover = None;
        Reply::repaint(true)
    }

    fn navigate(&mut self, section: &str, _select: Option<&str>) {
        if let Some(&s) = SECTIONS.iter().find(|s| s.name() == section) {
            self.open(s);
        }
    }

    fn cursor(&self, x: i32, y: i32, _w: i32, _h: i32) -> Cursor {
        match self.control_at(x, y) {
            Some(Control::Search) => Cursor::Text,
            Some(_) => Cursor::Hand,
            None => Cursor::Arrow,
        }
    }
}
