//! Desktop icons. The desktop starts out empty; right-clicking an app
//! (in Start or on the taskbar) or a file or folder (in Files) offers
//! "Create Shortcut", which puts one here. Shortcuts sit on a grid in the
//! work area and keep the cell you drag them to.
//!
//! They're saved, along with the taskbar's pinned apps, to
//! `/DESKTOP.CFG` on the disk -- one per line:
//!
//! ```text
//! pin terminal
//! app 0 0 sketch
//! file 0 1 /README.TXT
//! dir 1 0 /DOCS
//! ```
//!
//! (`col row` then the app id or absolute path, which may contain spaces),
//! plus `set <key> <value>` lines for Settings (see `settings.rs`).

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use super::apps::{file_icon, AppKind, accent, TEXT};
use super::assets;
use super::font;
use super::icon_ids as icon;
use super::surface::{rgb, Painter, Rect};

const CONFIG: &str = "/DESKTOP.CFG";

pub const CELL_W: i32 = 88;
pub const CELL_H: i32 = 96;
const TILE: i32 = 56;

#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    App(AppKind),
    /// An absolute path on the FAT16 disk.
    Dir(String),
    File(String),
}

pub struct Icon {
    pub target: Target,
    pub label: String,
    pub col: i32,
    pub row: i32,
    pub selected: bool,
}

impl Icon {
    pub fn new(target: Target, col: i32, row: i32) -> Self {
        let label = match &target {
            Target::App(k) => String::from(k.short_name()),
            Target::Dir(p) | Target::File(p) => String::from(p.rsplit('/').next().unwrap_or(p)),
        };
        Icon { target, label, col, row, selected: false }
    }
}

/// Reads `/DESKTOP.CFG`: the pinned apps and the shortcuts. Missing or
/// unreadable means a fresh desktop -- nothing pinned, no shortcuts.
pub fn load_config() -> (Vec<AppKind>, Vec<Icon>) {
    let (mut pinned, mut icons) = (Vec::new(), Vec::new());
    let Ok(data) = crate::vfs::read_file(CONFIG) else { return (pinned, icons) };
    // Not `String::from_utf8_lossy`: it needs unwinding support this
    // kernel can't link (see `cfile.rs`). We wrote the file, so it's UTF-8.
    let text = core::str::from_utf8(&data).unwrap_or("");
    for line in text.lines() {
        let mut f = line.trim().splitn(4, ' ');
        match (f.next(), f.next(), f.next(), f.next()) {
            (Some("set"), Some(key), Some(value), None) => {
                let mut s = super::settings::get();
                super::settings::parse_line(&mut s, key, value);
                super::settings::set(s);
            }
            (Some("pin"), Some(id), None, None) => {
                if let Some(k) = AppKind::from_id(id) {
                    if !pinned.contains(&k) {
                        pinned.push(k);
                    }
                }
            }
            (Some(kind), Some(col), Some(row), Some(rest)) => {
                let (Ok(col), Ok(row)) = (col.parse(), row.parse()) else { continue };
                let target = match kind {
                    "app" => match AppKind::from_id(rest) {
                        Some(k) => Target::App(k),
                        None => continue,
                    },
                    "file" => Target::File(String::from(rest)),
                    "dir" => Target::Dir(String::from(rest)),
                    _ => continue,
                };
                icons.push(Icon::new(target, col, row));
            }
            _ => {}
        }
    }
    (pinned, icons)
}

/// Writes the pinned apps and shortcuts back to `/DESKTOP.CFG`. Without
/// a disk this quietly does nothing; they just last until shutdown.
pub fn save_config(pinned: &[AppKind], icons: &[Icon]) {
    let mut out = String::new();
    super::settings::write_lines(&mut out);
    for k in pinned {
        let _ = writeln!(out, "pin {}", k.id());
    }
    for ic in icons {
        let _ = match &ic.target {
            Target::App(k) => writeln!(out, "app {} {} {}", ic.col, ic.row, k.id()),
            Target::File(p) => writeln!(out, "file {} {} {}", ic.col, ic.row, p),
            Target::Dir(p) => writeln!(out, "dir {} {} {}", ic.col, ic.row, p),
        };
    }
    let _ = crate::vfs::write_file(CONFIG, out.as_bytes());
}

/// How many rows of icons fit in `area`.
pub fn rows(area: Rect) -> i32 {
    ((area.h - 8) / CELL_H).max(1)
}

/// How many columns fit in `area`.
pub fn cols(area: Rect) -> i32 {
    ((area.w - 8) / CELL_W).max(1)
}

/// Moves any icon that's off the grid (the screen got smaller) or
/// sharing a cell onto the nearest free one.
pub fn tidy(icons: &mut [Icon], area: Rect) {
    let mut taken: Vec<(i32, i32)> = Vec::new();
    for ic in icons.iter_mut() {
        let on_grid = ic.col >= 0 && ic.row >= 0 && ic.col < cols(area) && ic.row < rows(area);
        if !on_grid || taken.contains(&(ic.col, ic.row)) {
            let (c, r) = nearest_free(area, ic.col.max(0), ic.row.max(0), &taken);
            ic.col = c;
            ic.row = r;
        }
        taken.push((ic.col, ic.row));
    }
}

/// Lays every icon out in order, column by column.
pub fn arrange(icons: &mut [Icon], area: Rect) {
    let rows = rows(area);
    for (i, ic) in icons.iter_mut().enumerate() {
        ic.col = i as i32 / rows;
        ic.row = i as i32 % rows;
    }
}

pub fn cell_rect(area: Rect, col: i32, row: i32) -> Rect {
    Rect::new(area.x + 4 + col * CELL_W, area.y + 4 + row * CELL_H, CELL_W, CELL_H)
}

/// The grid cell under screen point `(x, y)`, clamped onto the grid.
pub fn cell_at(area: Rect, x: i32, y: i32) -> (i32, i32) {
    let col = (x - area.x - 4).div_euclid(CELL_W).clamp(0, cols(area) - 1);
    let row = (y - area.y - 4).div_euclid(CELL_H).clamp(0, rows(area) - 1);
    (col, row)
}

/// The free cell closest to `(col, row)`, skipping the icons in `taken`.
pub fn nearest_free(area: Rect, col: i32, row: i32, taken: &[(i32, i32)]) -> (i32, i32) {
    let (cols, rows) = (cols(area), rows(area));
    let mut best = (col, row);
    let mut best_d = i32::MAX;
    for c in 0..cols {
        for r in 0..rows {
            if taken.contains(&(c, r)) {
                continue;
            }
            let d = (c - col) * (c - col) + (r - row) * (r - row);
            if d < best_d {
                best_d = d;
                best = (c, r);
            }
        }
    }
    best
}

/// The part of an icon's cell that reacts to the pointer: the tile and
/// its label, not the empty margins around them.
pub fn hit_rect(cell: Rect) -> Rect {
    Rect::new(cell.x + 6, cell.y + 2, cell.w - 12, cell.h - 6)
}

/// Draws icon `ic` in `cell`.
pub fn paint(p: &mut Painter, cell: Rect, ic: &Icon, hover: bool, alpha: u8) {
    let a = |v: u32| (v * alpha as u32 / 255) as u8;
    let hit = hit_rect(cell);
    if ic.selected {
        p.fill_squircle(hit, 14.0, accent(), a(105));
    } else if hover {
        p.fill_squircle(hit, 14.0, TEXT, a(34));
    }

    let tile = Rect::new(cell.x + (cell.w - TILE) / 2, cell.y + 8, TILE, TILE);
    p.fill_squircle(tile.offset(0, 3).expand(1), 18.0, rgb(0, 0, 0), a(48));
    let (tint, tint_a, glyph) = match &ic.target {
        Target::App(k) => (k.tint(), 240, k.taskbar_icons().1),
        Target::Dir(_) => (AppKind::Files.tint(), 240, icon::FILES_32_FILLED),
        Target::File(path) => {
            let glyph = match file_icon(path) {
                icon::DOCUMENT_TEXT_20 => icon::DOCUMENT_TEXT_32,
                icon::APP_20 => icon::APP_32,
                _ => icon::DOCUMENT_32,
            };
            (rgb(220, 228, 236), 92, glyph)
        }
    };
    p.fill_squircle(tile, 17.0, tint, a(tint_a));
    // A hairline of light along the top, like the glass panels' rim.
    p.fill_rect(Rect::new(tile.x + 14, tile.y + 1, tile.w - 28, 1), TEXT, a(70));
    let (iw, ih, m) = assets::icon(glyph);
    p.draw_mask(tile.x + (tile.w - iw) / 2, tile.y + (tile.h - ih) / 2, iw, ih, m, TEXT, alpha);

    // The label, shortened with an ellipsis if it doesn't fit.
    let f = &font::SMALL;
    let max = cell.w - 4;
    let mut label = ic.label.clone();
    if f.width(&label) > max {
        while !label.is_empty() && f.width(&label) + f.width("...") > max {
            label.pop();
        }
        label.push_str("...");
    }
    p.text_shadowed(f, cell.x + (cell.w - f.width(&label)) / 2, tile.bottom() + 8, &label, TEXT, alpha);
}
