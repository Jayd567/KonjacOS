//! The desktop's baked-in art (see `tools/gen_desktop_assets.py` for how
//! each blob was produced and its exact layout): the wallpaper, the "K"
//! boot logo, and Microsoft's Fluent System Icons (MIT) as 8-bit alpha
//! masks.

extern crate alloc;

use alloc::vec::Vec;

static WALLPAPER: &[u8] = include_bytes!("../../assets/wallpaper.rgb");
static LOGO: &[u8] = include_bytes!("../../assets/logo_k.a8");
static LOGO_MID: &[u8] = include_bytes!("../../assets/logo_k_mid.a8");
static LOGO_SMALL: &[u8] = include_bytes!("../../assets/logo_k_small.a8");
static ICONS: &[u8] = include_bytes!("../../assets/icons.kico");

fn u16_at(d: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([d[i], d[i + 1]])
}

/// An alpha mask: `(width, height, coverage bytes)`.
pub type Mask = (i32, i32, &'static [u8]);

fn mask(blob: &'static [u8]) -> Mask {
    (u16_at(blob, 4) as i32, u16_at(blob, 6) as i32, &blob[8..])
}

pub fn logo() -> Mask {
    mask(LOGO)
}

pub fn logo_mid() -> Mask {
    mask(LOGO_MID)
}

pub fn logo_small() -> Mask {
    mask(LOGO_SMALL)
}

/// Icon `id` (one of `icon_ids::*`) as a square mask.
pub fn icon(id: usize) -> Mask {
    let entry = 6 + id * 6;
    let size = u16_at(ICONS, entry) as usize;
    let off = u32::from_le_bytes([ICONS[entry + 2], ICONS[entry + 3], ICONS[entry + 4], ICONS[entry + 5]]) as usize;
    (size as i32, size as i32, &ICONS[off..off + size * size])
}

/// The wallpaper scaled to cover a `w x h` screen (centre-cropped,
/// bilinear), as `0x00RRGGBB` pixels. Baked at 1280x800, so on that
/// resolution -- QEMU's default here -- this is a straight copy.
pub fn wallpaper(w: i32, h: i32) -> Vec<u32> {
    let sw = u16_at(WALLPAPER, 4) as i32;
    let sh = u16_at(WALLPAPER, 6) as i32;
    let src = &WALLPAPER[8..];
    let px = |x: i32, y: i32| -> (u32, u32, u32) {
        let i = ((y.clamp(0, sh - 1) * sw + x.clamp(0, sw - 1)) * 3) as usize;
        (src[i] as u32, src[i + 1] as u32, src[i + 2] as u32)
    };
    let mut out = alloc::vec![0u32; (w * h) as usize];
    if w == sw && h == sh {
        for (i, p) in out.iter_mut().enumerate() {
            let (r, g, b) = px(i as i32 % sw, i as i32 / sw);
            *p = (r << 16) | (g << 8) | b;
        }
        return out;
    }
    // Cover: scale so both dimensions fill the screen, crop the overflow.
    // Fixed point, 8 fractional bits.
    let scale = ((sw << 16) / w).min((sh << 16) / h); // source px per screen px, 16.16
    let off_x = ((sw << 16) - scale * w) / 2;
    let off_y = ((sh << 16) - scale * h) / 2;
    for y in 0..h {
        let fy = off_y + y * scale;
        let (y0, ty) = (fy >> 16, ((fy >> 8) & 0xff) as u32);
        for x in 0..w {
            let fx = off_x + x * scale;
            let (x0, tx) = (fx >> 16, ((fx >> 8) & 0xff) as u32);
            let (a, b, c, d) = (px(x0, y0), px(x0 + 1, y0), px(x0, y0 + 1), px(x0 + 1, y0 + 1));
            let mix = |p: u32, q: u32, r: u32, s: u32| -> u32 {
                let top = p * (256 - tx) + q * tx;
                let bot = r * (256 - tx) + s * tx;
                (top * (256 - ty) + bot * ty) >> 16
            };
            out[(y * w + x) as usize] = (mix(a.0, b.0, c.0, d.0) << 16) | (mix(a.1, b.1, c.1, d.1) << 8) | mix(a.2, b.2, c.2, d.2);
        }
    }
    out
}
