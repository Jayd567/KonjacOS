//! Small pixel-pushing helper on top of the framebuffer Limine hands us,
//! plus glyph rendering using the embedded font in `font.rs`. `console.rs`
//! builds the actual scrolling text console on top of `draw_char`/`scroll_up`.

extern crate alloc;

use alloc::vec::Vec;

use crate::font::{FONT8X8, GLYPH_HEIGHT, GLYPH_WIDTH};
use crate::limine::Framebuffer;

pub struct Canvas {
    fb: Framebuffer,
}

// Safety: `Framebuffer` holds a raw pointer into Limine-provided VRAM, which
// isn't `Send` by default. This kernel is single-core with no threads, so
// there's no actual cross-thread sharing happening -- `Canvas` just needs
// to live inside `console::CONSOLE`, a `static`, which requires `Send`.
unsafe impl Send for Canvas {}

impl Canvas {
    pub fn new(fb: Framebuffer) -> Self {
        Canvas { fb }
    }

    pub fn width(&self) -> u64 {
        self.fb.width
    }

    pub fn height(&self) -> u64 {
        self.fb.height
    }

    /// Packs an 8-bit-per-channel RGB colour according to this
    /// framebuffer's actual channel layout (not every mode is 0xRRGGBB).
    #[inline]
    fn pack(&self, r: u8, g: u8, b: u8) -> u32 {
        (u32::from(r) << self.fb.red_mask_shift)
            | (u32::from(g) << self.fb.green_mask_shift)
            | (u32::from(b) << self.fb.blue_mask_shift)
    }

    /// # Safety
    /// `x < width` and `y < height` must hold; this does no bounds checking
    /// so the hot paths below (fill, gradient) can stay branch-free.
    #[inline]
    unsafe fn put_pixel_unchecked(&mut self, x: u64, y: u64, colour: u32) {
        let offset = y * self.fb.pitch + x * (u64::from(self.fb.bpp) / 8);
        unsafe {
            let ptr = self.fb.address.add(offset as usize) as *mut u32;
            ptr.write_volatile(colour);
        }
    }

    pub fn put_pixel(&mut self, x: u64, y: u64, r: u8, g: u8, b: u8) {
        if x >= self.fb.width || y >= self.fb.height {
            return;
        }
        let colour = self.pack(r, g, b);
        unsafe { self.put_pixel_unchecked(x, y, colour) };
    }

    pub fn fill_rect(&mut self, x0: u64, y0: u64, w: u64, h: u64, r: u8, g: u8, b: u8) {
        let colour = self.pack(r, g, b);
        let x1 = core::cmp::min(x0 + w, self.fb.width);
        let y1 = core::cmp::min(y0 + h, self.fb.height);
        for y in y0..y1 {
            for x in x0..x1 {
                unsafe { self.put_pixel_unchecked(x, y, colour) };
            }
        }
    }

    pub fn clear(&mut self, r: u8, g: u8, b: u8) {
        self.fill_rect(0, 0, self.fb.width, self.fb.height, r, g, b);
    }

    /// Draws a small "boot splash": a dark background, a horizontal
    /// gradient bar, and a border -- enough to see at a glance that the
    /// framebuffer, pixel format, and pitch were all parsed correctly.
    pub fn draw_boot_splash(&mut self) {
        self.clear(18, 18, 24);

        let bar_y = self.height() / 2 - 20;
        let bar_h = 40;
        let w = self.width();
        for x in 0..w {
            let t = x as f32 / w as f32;
            let r = (20.0 + t * 100.0) as u8;
            let g = (80.0 + t * 120.0) as u8;
            let b = (200.0 - t * 80.0) as u8;
            for y in bar_y..bar_y + bar_h {
                self.put_pixel(x, y, r, g, b);
            }
        }

        let border = 4;
        let (w, h) = (self.width(), self.height());
        self.fill_rect(0, 0, w, border, 240, 240, 240);
        self.fill_rect(0, h - border, w, border, 240, 240, 240);
        self.fill_rect(0, 0, border, h, 240, 240, 240);
        self.fill_rect(w - border, 0, border, h, 240, 240, 240);
    }

    /// Draws one glyph from the embedded 8x8 font at pixel position
    /// `(x, y)` (its top-left corner), `fg` for set bits and `bg` for
    /// unset ones. Non-ASCII or unmapped codepoints fall back to a space.
    pub fn draw_char(&mut self, x: u64, y: u64, c: char, fg: (u8, u8, u8), bg: (u8, u8, u8)) {
        let index = if (c as u32) < 128 { c as usize } else { 0 };
        let glyph = &FONT8X8[index];
        let fg = self.pack(fg.0, fg.1, fg.2);
        let bg = self.pack(bg.0, bg.1, bg.2);
        for row in 0..GLYPH_HEIGHT {
            let bits = glyph[row];
            let py = y + row as u64;
            if py >= self.fb.height {
                break;
            }
            for col in 0..GLYPH_WIDTH {
                let px = x + col as u64;
                if px >= self.fb.width {
                    break;
                }
                let set = (bits >> col) & 1 != 0;
                unsafe { self.put_pixel_unchecked(px, py, if set { fg } else { bg }) };
            }
        }
    }

    /// Inverts every pixel in the given rectangle. Calling this twice on
    /// the same rectangle restores the original pixels exactly -- that
    /// self-cancelling property is what makes it a good way to draw a
    /// blinking cursor without needing to remember what was underneath it.
    pub fn invert_rect(&mut self, x0: u64, y0: u64, w: u64, h: u64) {
        let x1 = core::cmp::min(x0 + w, self.fb.width);
        let y1 = core::cmp::min(y0 + h, self.fb.height);
        let bypp = u64::from(self.fb.bpp) / 8;
        for y in y0..y1 {
            for x in x0..x1 {
                let offset = y * self.fb.pitch + x * bypp;
                unsafe {
                    let ptr = self.fb.address.add(offset as usize) as *mut u32;
                    let current = ptr.read_volatile();
                    ptr.write_volatile(!current);
                }
            }
        }
    }

    /// Unpacks the raw colour value that a previous `pack`/write left at
    /// `(x, y)` back into 8-bit channels, using this framebuffer's own
    /// channel shifts (never assume a fixed byte order -- see `pack`).
    #[inline]
    unsafe fn get_pixel_unchecked(&self, x: u64, y: u64) -> (u8, u8, u8) {
        let offset = y * self.fb.pitch + x * (u64::from(self.fb.bpp) / 8);
        let colour = unsafe {
            let ptr = self.fb.address.add(offset as usize) as *const u32;
            ptr.read_volatile()
        };
        let r = (colour >> self.fb.red_mask_shift) as u8;
        let g = (colour >> self.fb.green_mask_shift) as u8;
        let b = (colour >> self.fb.blue_mask_shift) as u8;
        (r, g, b)
    }

    /// Alpha-blends `(r, g, b)` over whatever is already on screen at
    /// `(x, y)`, weighted by `alpha` (0 = leave untouched, 255 = fully
    /// opaque). `wm.rs` uses this to draw the real cursor-pack bitmap
    /// (which carries a genuine per-pixel alpha channel, not just a 1-bit
    /// mask) with proper soft edges instead of a hard-edged cutout.
    pub fn blend_pixel(&mut self, x: u64, y: u64, r: u8, g: u8, b: u8, alpha: u8) {
        if x >= self.fb.width || y >= self.fb.height || alpha == 0 {
            return;
        }
        if alpha == 255 {
            self.put_pixel(x, y, r, g, b);
            return;
        }
        let (br, bg, bb) = unsafe { self.get_pixel_unchecked(x, y) };
        let a = u16::from(alpha);
        let inv = 255 - a;
        let out_r = ((u16::from(r) * a + u16::from(br) * inv) / 255) as u8;
        let out_g = ((u16::from(g) * a + u16::from(bg) * inv) / 255) as u8;
        let out_b = ((u16::from(b) * a + u16::from(bb) * inv) / 255) as u8;
        unsafe { self.put_pixel_unchecked(x, y, self.pack(out_r, out_g, out_b)) };
    }

    /// Copies every raw pixel byte currently on screen out into a `Vec`.
    /// `wm.rs` uses this to freeze "the desktop" once before drawing
    /// floating windows over it, so each frame can cheaply restore a clean
    /// background by blitting this back rather than tracking exactly what
    /// moved and needs erasing.
    pub fn snapshot(&self) -> Vec<u8> {
        let len = (self.fb.pitch * self.fb.height) as usize;
        let mut buf = alloc::vec![0u8; len];
        unsafe {
            core::ptr::copy_nonoverlapping(self.fb.address, buf.as_mut_ptr(), len);
        }
        buf
    }

    /// Writes a buffer from [`snapshot`](Self::snapshot) back to the
    /// framebuffer verbatim.
    pub fn blit(&mut self, data: &[u8]) {
        let len = ((self.fb.pitch * self.fb.height) as usize).min(data.len());
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), self.fb.address, len);
        }
    }

    /// Scrolls the whole framebuffer up by `rows_px` pixels (a memmove of
    /// the pixel data, so it's proportional to screen size but still just
    /// one pass), filling the newly-exposed bottom strip with `bg`.
    pub fn scroll_up(&mut self, rows_px: u64, bg: (u8, u8, u8)) {
        let rows_px = core::cmp::min(rows_px, self.fb.height);
        let row_bytes = self.fb.pitch as usize;
        let move_rows = (self.fb.height - rows_px) as usize;
        if move_rows > 0 {
            unsafe {
                let base = self.fb.address;
                let dst = base;
                let src = base.add(rows_px as usize * row_bytes);
                core::ptr::copy(src, dst, move_rows * row_bytes);
            }
        }
        self.fill_rect(0, self.fb.height - rows_px, self.fb.width, rows_px, bg.0, bg.1, bg.2);
    }
}
