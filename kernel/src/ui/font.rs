//! Anti-aliased bitmap fonts: glyph atlases rasterised offline from real
//! TrueType fonts (Inter, JetBrains Mono -- both SIL OFL) by
//! `tools/gen_desktop_assets.py`, each glyph an 8-bit coverage mask rather
//! than the console's 1-bit 8x8 cells, so edges blend softly into the glass
//! underneath them.
//!
//! Layout ("KFNT"): magic, u16 size, u16 ascent, u16 descent, u16 first
//! char, u16 count, then `count` 16-byte records `{i16 left, i16 top,
//! u16 w, u16 h, u16 advance (1/16 px), u16 pad, u32 offset}`, then the
//! coverage bytes. `top` is measured down from the line's top.

pub struct Font(&'static [u8]);

pub struct Glyph {
    pub left: i16,
    pub top: i16,
    pub w: u16,
    pub h: u16,
    pub advance: u16,
    pub bitmap: &'static [u8],
}

fn u16_at(d: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([d[i], d[i + 1]])
}

impl Font {
    pub const fn new(data: &'static [u8]) -> Self {
        Font(data)
    }

    pub fn line_height(&self) -> i32 {
        u16_at(self.0, 6) as i32 + u16_at(self.0, 8) as i32
    }

    pub fn glyph(&self, ch: char) -> Glyph {
        let d = self.0;
        let first = u16_at(d, 10) as u32;
        let count = u16_at(d, 12) as u32;
        let code = ch as u32;
        let index = if code >= first && code < first + count { code - first } else { b'?' as u32 - first };
        let rec = 14 + index as usize * 16;
        let w = u16_at(d, rec + 4);
        let h = u16_at(d, rec + 6);
        let off = u32::from_le_bytes([d[rec + 12], d[rec + 13], d[rec + 14], d[rec + 15]]) as usize;
        Glyph {
            left: u16_at(d, rec) as i16,
            top: u16_at(d, rec + 2) as i16,
            w,
            h,
            advance: u16_at(d, rec + 8),
            bitmap: &d[off..off + w as usize * h as usize],
        }
    }

    /// Width of `text` in whole pixels.
    pub fn width(&self, text: &str) -> i32 {
        let sum: u32 = text.chars().map(|c| self.glyph(c).advance as u32).sum();
        ((sum + 15) / 16) as i32
    }

    /// The advance of one character, for the fixed-pitch terminal grid.
    pub fn cell_width(&self) -> i32 {
        (self.glyph('M').advance as i32 + 15) / 16
    }
}

pub static UI: Font = Font::new(include_bytes!("../../assets/font_ui.kfnt"));
pub static UI_BOLD: Font = Font::new(include_bytes!("../../assets/font_ui_bold.kfnt"));
pub static SMALL: Font = Font::new(include_bytes!("../../assets/font_small.kfnt"));
pub static DISPLAY: Font = Font::new(include_bytes!("../../assets/font_display.kfnt"));
pub static MONO: Font = Font::new(include_bytes!("../../assets/font_mono.kfnt"));
