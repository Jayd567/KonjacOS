//! Settings: the desktop's preferences and the app that edits them.
//!
//! The current [`Settings`] live in one global, bumped to a new
//! [`revision`] on every change; the desktop watches that, applies what
//! changed (a new wallpaper, recomputed glass, a reformatted clock) and
//! saves everything to `/DESKTOP.CFG` as `set <key> <value>` lines next
//! to the pins and shortcuts (see `icons.rs`).

extern crate alloc;

use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::apps::{App, MouseEvent, Reply, TEXT, TEXT_DIM};
use super::assets;
use super::font;
use super::surface::{rgb, Painter, Rect};
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

const THUMB_W: i32 = 112;
const THUMB_H: i32 = 70;
const PAD: i32 = 28;
const SEG_H: i32 = 34;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Control {
    Wallpaper(u8),
    Accent(u8),
    Glass(u8),
    Pointer(u8),
    DoubleClick(u8),
    Clock(u8),
}

/// The rows below the wallpapers and accents: a heading, then a
/// segmented control of options.
const ROWS: [(&str, [&str; 3]); 4] = [
    ("Glass", ["Clear", "Balanced", "Frosted"]),
    ("Pointer speed", ["Slow", "Normal", "Fast"]),
    ("Double-click speed", ["Slow", "Normal", "Fast"]),
    ("Clock", ["12-hour", "24-hour", ""]),
];

pub struct SettingsApp {
    thumbs: Vec<Vec<u32>>,
    hover: Option<Control>,
}

impl SettingsApp {
    pub fn new() -> Self {
        let thumbs = (0..WALLPAPERS.len() as u8).map(|i| wallpaper(i, THUMB_W, THUMB_H)).collect();
        SettingsApp { thumbs, hover: None }
    }

    fn thumb_rect(i: u8) -> Rect {
        Rect::new(PAD + i as i32 * (THUMB_W + 14), 40, THUMB_W, THUMB_H)
    }

    fn accent_rect(i: u8) -> Rect {
        Rect::new(PAD + i as i32 * 46, 176, 32, 32)
    }

    fn row_y(row: usize) -> i32 {
        234 + row as i32 * 76
    }

    fn segment_rect(row: usize, i: u8) -> Rect {
        Rect::new(PAD + i as i32 * 128, Self::row_y(row) + 24, 124, SEG_H)
    }

    fn controls() -> impl Iterator<Item = (Control, Rect)> {
        let wall = (0..WALLPAPERS.len() as u8).map(|i| (Control::Wallpaper(i), Self::thumb_rect(i)));
        let acc = (0..ACCENTS.len() as u8).map(|i| (Control::Accent(i), Self::accent_rect(i)));
        let segs = (0..ROWS.len()).flat_map(|row| {
            (0..3u8).filter(move |&i| !ROWS[row].1[i as usize].is_empty()).map(move |i| {
                let c = match row {
                    0 => Control::Glass(i),
                    1 => Control::Pointer(i),
                    2 => Control::DoubleClick(i),
                    _ => Control::Clock(i),
                };
                (c, Self::segment_rect(row, i))
            })
        });
        wall.chain(acc).chain(segs)
    }

    fn control_at(x: i32, y: i32) -> Option<Control> {
        Self::controls().find(|(_, r)| r.contains(x, y)).map(|(c, _)| c)
    }

    fn selected(s: &Settings, c: Control) -> bool {
        match c {
            Control::Wallpaper(i) => s.wallpaper == i,
            Control::Accent(i) => s.accent == i,
            Control::Glass(i) => s.glass == i,
            Control::Pointer(i) => s.pointer == i,
            Control::DoubleClick(i) => s.double_click == i,
            Control::Clock(i) => s.clock_24h == (i == 1),
        }
    }
}

fn heading(p: &mut Painter, y: i32, text: &str) {
    p.text(&font::SMALL, PAD, y, text, TEXT_DIM, 230);
}

impl App for SettingsApp {
    fn client_size(&self) -> (i32, i32) {
        (THUMB_W * 4 + 14 * 3 + 2 * PAD, 548)
    }

    fn resizable(&self) -> bool {
        false
    }

    fn paint(&mut self, p: &mut Painter, w: i32, h: i32) {
        let s = get();
        let acc = accent();
        p.fill_squircle(Rect::new(10, 0, w - 20, h - 10), 20.0, rgb(4, 8, 12), 70);

        heading(p, 16, "WALLPAPER");
        for i in 0..WALLPAPERS.len() as u8 {
            let r = Self::thumb_rect(i);
            if s.wallpaper == i {
                p.fill_squircle(r.expand(4), 14.0, acc, 255);
            } else if self.hover == Some(Control::Wallpaper(i)) {
                p.fill_squircle(r.expand(4), 14.0, TEXT, 70);
            }
            p.blit(r.x, r.y, r.w, r.h, &self.thumbs[i as usize], THUMB_W);
            let name = WALLPAPERS[i as usize];
            p.text(&font::SMALL, r.x + (r.w - font::SMALL.width(name)) / 2, r.bottom() + 10, name, TEXT, 255);
        }

        heading(p, 152, "ACCENT COLOUR");
        for (i, &(_, c)) in ACCENTS.iter().enumerate() {
            let r = Self::accent_rect(i as u8);
            if s.accent == i as u8 {
                p.fill_squircle(r.expand(4), 14.0, TEXT, 255);
            } else if self.hover == Some(Control::Accent(i as u8)) {
                p.fill_squircle(r.expand(4), 14.0, TEXT, 90);
            }
            p.fill_squircle(r, 11.0, c, 255);
        }

        for (row, (title, options)) in ROWS.iter().enumerate() {
            let y = Self::row_y(row);
            let mut upper = alloc::string::String::new();
            upper.extend(title.chars().map(|c| c.to_ascii_uppercase()));
            heading(p, y, &upper);
            let n = options.iter().filter(|o| !o.is_empty()).count() as u8;
            let track = Self::segment_rect(row, 0).union(&Self::segment_rect(row, n - 1)).expand(3);
            p.fill_squircle(track, 13.0, rgb(0, 0, 0), 70);
            for i in 0..n {
                let c = match row {
                    0 => Control::Glass(i),
                    1 => Control::Pointer(i),
                    2 => Control::DoubleClick(i),
                    _ => Control::Clock(i),
                };
                let r = Self::segment_rect(row, i);
                let on = Self::selected(&s, c);
                if on {
                    p.fill_squircle(r, 10.0, acc, 235);
                } else if self.hover == Some(c) {
                    p.fill_squircle(r, 10.0, TEXT, 26);
                }
                let label = options[i as usize];
                let color = if on { rgb(14, 22, 26) } else { TEXT };
                p.text(&font::UI, r.x + (r.w - font::UI.width(label)) / 2, r.y + (SEG_H - font::UI.line_height()) / 2, label, color, 255);
            }
        }
    }

    fn mouse(&mut self, ev: MouseEvent, x: i32, y: i32, _w: i32, _h: i32) -> Reply {
        let over = Self::control_at(x, y);
        match ev {
            MouseEvent::Move => {
                let changed = over != self.hover;
                self.hover = over;
                Reply::repaint(changed)
            }
            MouseEvent::Down | MouseEvent::DoubleClick => {
                let Some(c) = over else { return Reply::default() };
                let mut s = get();
                match c {
                    Control::Wallpaper(i) => s.wallpaper = i,
                    Control::Accent(i) => s.accent = i,
                    Control::Glass(i) => s.glass = i,
                    Control::Pointer(i) => s.pointer = i,
                    Control::DoubleClick(i) => s.double_click = i,
                    Control::Clock(i) => s.clock_24h = i == 1,
                }
                if s != get() {
                    set(s);
                }
                Reply::repaint(true)
            }
            _ => Reply::default(),
        }
    }

    fn cursor(&self, x: i32, y: i32, _w: i32, _h: i32) -> Cursor {
        if Self::control_at(x, y).is_some() {
            Cursor::Hand
        } else {
            Cursor::Arrow
        }
    }
}
