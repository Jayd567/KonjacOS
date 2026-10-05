//! Desktop icons: every app, plus whatever is in the root of the disk.
//! They sit on a grid in the work area, filled column by column from the
//! top left like other desktops, and keep the cell you drag them to.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use super::apps::{file_icon, AppKind, ACCENT, TEXT};
use super::assets;
use super::font;
use super::icon_ids as icon;
use super::surface::{rgb, Painter, Rect};

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
    pub fn app(&self) -> Option<AppKind> {
        match self.target {
            Target::App(k) => Some(k),
            _ => None,
        }
    }
}

/// The icons for the desktop: apps first, then folders, then files.
pub fn load() -> Vec<Icon> {
    let mut icons: Vec<Icon> = AppKind::ALL
        .iter()
        .map(|&k| Icon { target: Target::App(k), label: String::from(k.short_name()), col: 0, row: 0, selected: false })
        .collect();
    if let Ok(mut list) = crate::fat16::list_dir("/") {
        list.retain(|e| e.name != "." && e.name != "..");
        list.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase())));
        for e in list {
            let mut path = String::from("/");
            path.push_str(&e.name);
            let target = if e.is_dir { Target::Dir(path) } else { Target::File(path) };
            icons.push(Icon { target, label: e.name, col: 0, row: 0, selected: false });
        }
    }
    icons
}

/// How many rows of icons fit in `area`.
pub fn rows(area: Rect) -> i32 {
    ((area.h - 8) / CELL_H).max(1)
}

/// How many columns fit in `area`.
pub fn cols(area: Rect) -> i32 {
    ((area.w - 8) / CELL_W).max(1)
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
        p.fill_squircle(hit, 14.0, ACCENT, a(105));
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
