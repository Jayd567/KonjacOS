//! Small pixel-pushing helper on top of the framebuffer Limine hands us,
//! plus glyph rendering using the embedded font in `font.rs`. `console.rs`
//! builds the actual scrolling text console on top of `draw_char`/`scroll_up`.

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

    /// Draws the boot logo: the three-bar "K" (see `ui/assets.rs`),
    /// white on black, centred. The desktop picks up from exactly this
    /// frame for its boot animation.
    pub fn draw_boot_logo(&mut self) {
        self.clear(0, 0, 0);
        let (lw, lh, mask) = crate::ui::assets::logo();
        let x0 = (self.width() as i32 - lw) / 2;
        let y0 = (self.height() as i32 - lh) / 2;
        for y in 0..lh {
            for x in 0..lw {
                let a = mask[(y * lw + x) as usize];
                if a != 0 && x0 + x >= 0 && y0 + y >= 0 {
                    self.put_pixel((x0 + x) as u64, (y0 + y) as u64, a, a, a);
                }
            }
        }
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

    /// Copies the `(x, y, w, h)` rectangle of an off-screen `0x00RRGGBB`
    /// buffer (`stride` pixels per row, same size as the screen) to the
    /// framebuffer -- how the desktop puts each finished frame on screen,
    /// one changed region at a time. A plain row copy when the
    /// framebuffer's layout is the usual 0x00RRGGBB, repacked per pixel
    /// otherwise.
    pub fn present(&mut self, src: &[u32], stride: usize, x: usize, y: usize, w: usize, h: usize) {
        let w = w.min((self.fb.width as usize).saturating_sub(x));
        let h = h.min((self.fb.height as usize).saturating_sub(y));
        let native = self.fb.bpp == 32 && self.fb.red_mask_shift == 16 && self.fb.green_mask_shift == 8 && self.fb.blue_mask_shift == 0;
        for row in y..y + h {
            let line = &src[row * stride + x..row * stride + x + w];
            let dst = unsafe { self.fb.address.add(row * self.fb.pitch as usize + x * 4) as *mut u32 };
            if native {
                unsafe { core::ptr::copy_nonoverlapping(line.as_ptr(), dst, w) };
            } else {
                for (i, &p) in line.iter().enumerate() {
                    let c = self.pack((p >> 16) as u8, (p >> 8) as u8, p as u8);
                    unsafe { dst.add(i).write_volatile(c) };
                }
            }
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
