//! Off-screen drawing: [`Surface`] is a plain `0x00RRGGBB` pixel buffer
//! (the desktop composes everything into one, then copies only what
//! changed to the real framebuffer -- reading VRAM back is far too slow to
//! blend against directly), and [`Painter`] is a clipped, translated view
//! of one that every drawing call goes through.

extern crate alloc;

use alloc::vec::Vec;

use super::font::Font;
use super::math::sqrt;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.right().min(o.right());
        let y1 = self.bottom().min(o.bottom());
        Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }

    pub fn intersects(&self, o: &Rect) -> bool {
        !self.intersect(o).is_empty()
    }

    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = self.right().max(o.right());
        let y1 = self.bottom().max(o.bottom());
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn expand(&self, by: i32) -> Rect {
        Rect::new(self.x - by, self.y - by, self.w + 2 * by, self.h + 2 * by)
    }

    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }
}

pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// `src` over `dst` at opacity `a` (0..=255), all three channels at once:
/// red and blue share one multiply (they're 16 bits apart, so the
/// products can't collide), green gets its own.
#[inline]
pub fn blend(dst: u32, src: u32, a: u32) -> u32 {
    if a >= 255 {
        return src;
    }
    if a == 0 {
        return dst;
    }
    let ia = 255 - a;
    let rb = ((src & 0xff00ff) * a + (dst & 0xff00ff) * ia + 0x800080) >> 8;
    let g = ((src & 0x00ff00) * a + (dst & 0x00ff00) * ia + 0x008000) >> 8;
    (rb & 0xff00ff) | (g & 0x00ff00)
}

pub struct Surface {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u32>,
}

impl Surface {
    pub fn new(w: i32, h: i32) -> Self {
        Surface { w, h, px: alloc::vec![0; (w * h) as usize] }
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.w, self.h)
    }

    pub fn painter(&mut self, clip: Rect) -> Painter<'_> {
        let clip = clip.intersect(&self.bounds());
        Painter { s: self, clip, ox: 0, oy: 0, alpha: 255 }
    }
}

/// A clipped, translated, optionally faded window onto a [`Surface`].
/// Coordinates passed to every method are relative to the painter's
/// origin; `alpha` scales the opacity of everything drawn through it (used
/// for fade-in animations).
pub struct Painter<'a> {
    s: &'a mut Surface,
    clip: Rect,
    ox: i32,
    oy: i32,
    pub alpha: u8,
}

impl<'a> Painter<'a> {
    /// A sub-painter whose origin is `r`'s top-left and whose clip is the
    /// intersection of `r` with the current clip.
    pub fn sub(&mut self, r: Rect) -> Painter<'_> {
        let abs = r.offset(self.ox, self.oy);
        Painter { clip: self.clip.intersect(&abs), ox: abs.x, oy: abs.y, alpha: self.alpha, s: self.s }
    }

    #[inline]
    fn scale(&self, a: u32) -> u32 {
        (a * self.alpha as u32 * 257 + 0x8000) >> 16
    }

    pub fn fill_rect(&mut self, r: Rect, color: u32, alpha: u8) {
        let a = self.scale(alpha as u32);
        let r = r.offset(self.ox, self.oy).intersect(&self.clip);
        if r.is_empty() || a == 0 {
            return;
        }
        let w = self.s.w;
        for y in r.y..r.bottom() {
            let row = &mut self.s.px[(y * w + r.x) as usize..(y * w + r.right()) as usize];
            for p in row {
                *p = blend(*p, color, a);
            }
        }
    }

    /// A filled squircle (rounded rectangle with superellipse corners --
    /// see `glass.rs` for why not circular arcs), anti-aliased from its
    /// signed distance field. Used for hover boxes, pills and buttons.
    pub fn fill_squircle(&mut self, r: Rect, radius: f32, color: u32, alpha: u8) {
        let a_max = self.scale(alpha as u32);
        let abs = r.offset(self.ox, self.oy);
        let c = abs.intersect(&self.clip);
        if c.is_empty() || a_max == 0 {
            return;
        }
        let hw = r.w as f32 * 0.5;
        let hh = r.h as f32 * 0.5;
        let radius = radius.min(hw).min(hh);
        let w = self.s.w;
        // Only the four corners need the (float) SDF; everything else is a
        // straight-edged, fully covered span.
        let band = radius as i32 + 1;
        for y in c.y..c.bottom() {
            let ly = y - abs.y;
            let (x0, x1) = if ly >= band && ly < r.h - band {
                (c.x, c.right())
            } else {
                ((abs.x + band).max(c.x), (abs.right() - band).min(c.right()))
            };
            let row = (y * w) as usize;
            for x in x0..x1 {
                self.s.px[row + x as usize] = blend(self.s.px[row + x as usize], color, a_max);
            }
            let py = ly as f32 + 0.5 - hh;
            for x in (c.x..c.right()).filter(|&x| x < x0 || x >= x1.max(x0)) {
                let px = (x - abs.x) as f32 + 0.5 - hw;
                let d = squircle_sdf(px, py, hw, hh, radius);
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let i = (y * w + x) as usize;
                self.s.px[i] = blend(self.s.px[i], color, (a_max as f32 * cov) as u32);
            }
        }
    }

    /// Blends an 8-bit coverage mask (icon, logo, glyph) in `color`.
    pub fn draw_mask(&mut self, x: i32, y: i32, w: i32, h: i32, mask: &[u8], color: u32, alpha: u8) {
        let a_max = self.scale(alpha as u32);
        let abs = Rect::new(x + self.ox, y + self.oy, w, h);
        let c = abs.intersect(&self.clip);
        if c.is_empty() || a_max == 0 {
            return;
        }
        let sw = self.s.w;
        for yy in c.y..c.bottom() {
            let mrow = ((yy - abs.y) * w) as usize;
            for xx in c.x..c.right() {
                let m = mask[mrow + (xx - abs.x) as usize] as u32;
                if m == 0 {
                    continue;
                }
                let i = (yy * sw + xx) as usize;
                self.s.px[i] = blend(self.s.px[i], color, (m * a_max * 257 + 0x8000) >> 16);
            }
        }
    }

    /// Copies opaque `0x00RRGGBB` pixels (DOOM's frame, the wallpaper).
    pub fn blit(&mut self, x: i32, y: i32, w: i32, h: i32, src: &[u32], stride: i32) {
        let abs = Rect::new(x + self.ox, y + self.oy, w, h);
        let c = abs.intersect(&self.clip);
        if c.is_empty() {
            return;
        }
        let sw = self.s.w;
        let a = self.scale(255);
        for yy in c.y..c.bottom() {
            let s0 = ((yy - abs.y) * stride + (c.x - abs.x)) as usize;
            let d0 = (yy * sw + c.x) as usize;
            let n = c.w as usize;
            if a >= 255 {
                self.s.px[d0..d0 + n].copy_from_slice(&src[s0..s0 + n]);
            } else {
                for i in 0..n {
                    self.s.px[d0 + i] = blend(self.s.px[d0 + i], src[s0 + i], a);
                }
            }
        }
    }

    /// Draws `text` with its top-left at `(x, y)`; returns the advance in
    /// pixels.
    pub fn text(&mut self, font: &Font, x: i32, y: i32, text: &str, color: u32, alpha: u8) -> i32 {
        let mut pen = x * 16;
        for ch in text.chars() {
            let g = font.glyph(ch);
            let gx = (pen + 8) / 16 + g.left as i32;
            let gy = y + g.top as i32;
            if g.w > 0 {
                self.draw_mask(gx, gy, g.w as i32, g.h as i32, g.bitmap, color, alpha);
            }
            pen += g.advance as i32;
        }
        (pen - x * 16 + 15) / 16
    }

    /// Text on glass: a soft, faint dark copy one pixel down (a cheap
    /// three-tap blur standing in for a real Gaussian), then the crisp text
    /// on top -- legible over light and dark backdrops alike.
    pub fn text_shadowed(&mut self, font: &Font, x: i32, y: i32, text: &str, color: u32, alpha: u8) -> i32 {
        let shadow = rgb(0, 0, 0);
        let a = alpha as u32;
        self.text(font, x, y + 1, text, shadow, (a * 70 / 255) as u8);
        self.text(font, x - 1, y + 1, text, shadow, (a * 22 / 255) as u8);
        self.text(font, x + 1, y + 1, text, shadow, (a * 22 / 255) as u8);
        self.text(font, x, y + 2, text, shadow, (a * 22 / 255) as u8);
        self.text(font, x, y, text, color, alpha)
    }
}

/// Signed distance (negative inside) from `(px, py)` -- relative to the
/// shape's centre -- to a `2*hw x 2*hh` rectangle whose corners are
/// superellipse (L4-norm) arcs of radius `r` instead of circular ones.
/// An L4 corner meets the straight edge with zero curvature on both
/// sides, so curvature is continuous all the way round (G2) -- no visible
/// "tangent kink" where a circular arc would meet the line.
#[inline]
pub fn squircle_sdf(px: f32, py: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = px.abs() - (hw - r);
    let qy = py.abs() - (hh - r);
    if qx > 0.0 && qy > 0.0 {
        let x2 = qx * qx;
        let y2 = qy * qy;
        sqrt(sqrt(x2 * x2 + y2 * y2)) - r
    } else {
        qx.max(qy) - r
    }
}
