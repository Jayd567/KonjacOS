//! The liquid glass material every panel on the desktop is made of.
//!
//! Per panel, in order:
//!
//! 1. **Shape** -- a squircle signed distance field (see
//!    [`squircle_sdf`]): coverage for anti-aliased edges, the outward
//!    surface normal from the SDF's gradient, and the distance to the edge.
//! 2. **Blur** -- the backdrop under the panel (whatever has already been
//!    composed below it) is downsampled and box-blurred three times (a
//!    close Gaussian approximation). The radius depends on how high the
//!    panel floats: ~10px for a tooltip, 40px for a window.
//! 3. **Refraction** -- inside a bezel band along the edge, the glass is a
//!    curved lens. A squircle height profile gives the surface slope; Snell's
//!    law (n = 1.5) gives how far a vertical ray is bent; the backdrop is
//!    sampled that far inward along the normal. Red and blue bend by
//!    slightly different amounts (dispersion), giving the faint rainbow rim.
//! 4. **Adaptive tint** -- the backdrop's average colour is injected back in
//!    (orange wallpaper, orange-tinted glass), and the frost shifts darker
//!    over light backdrops / lighter over dark ones to keep white text
//!    legible either way.
//! 5. **Light** -- a 1px interior rim, bright at the top fading to nothing
//!    at the bottom (a physical top-edge light catch), plus a specular glint
//!    along the bezel facing the light.
//! 6. **Dither** -- ~3% monochrome noise, which hides gradient banding.
//!
//! Everything shape-dependent is computed once per size and cached in
//! [`Shape`]; everything backdrop-dependent is cached per panel in
//! [`Glass`] and only recomputed when the panel moves or something beneath
//! it changes (the desktop tracks that -- see `desktop.rs`).

extern crate alloc;

use alloc::vec::Vec;

use super::math::{hash2, smoothstep, sqrt};
use super::surface::{blend, squircle_sdf, Rect, Surface};

#[derive(Clone, Copy)]
pub struct GlassStyle {
    /// Corner radius of the squircle.
    pub radius: f32,
    /// Blur radius in pixels -- the panel's "height" above the desktop.
    pub blur: i32,
    /// Width of the curved, refracting band along the edge.
    pub bezel: f32,
    /// Maximum inward displacement of the backdrop at the rim, in pixels.
    pub refraction: f32,
    /// How much of the backdrop's average colour to inject.
    pub tint: f32,
    /// Constant darkening (windows need a calmer surface behind content).
    pub base_dark: f32,
    /// Extra darkening allowed over light backdrops.
    pub frost_dark: f32,
    /// Extra lightening allowed over dark backdrops.
    pub frost_light: f32,
    /// Saturation multiplier for the backdrop ("vibrancy").
    pub saturation: f32,
    /// How sharp (unblurred) the refracted backdrop is right at the rim.
    pub edge_sharp: f32,
    /// Drop shadow spread in pixels (0 = none).
    pub shadow: i32,
    pub shadow_alpha: f32,
}

pub const TASKBAR: GlassStyle = GlassStyle {
    radius: 30.0,
    blur: 24,
    bezel: 20.0,
    refraction: 22.0,
    tint: 0.14,
    base_dark: 0.02,
    frost_dark: 0.16,
    frost_light: 0.10,
    saturation: 1.45,
    edge_sharp: 0.85,
    shadow: 18,
    shadow_alpha: 0.26,
};

pub const WINDOW: GlassStyle = GlassStyle {
    radius: 48.0,
    blur: 40,
    bezel: 18.0,
    refraction: 20.0,
    tint: 0.16,
    base_dark: 0.20,
    frost_dark: 0.26,
    frost_light: 0.06,
    saturation: 1.35,
    edge_sharp: 0.75,
    shadow: 30,
    shadow_alpha: 0.36,
};

pub const MENU: GlassStyle = GlassStyle {
    radius: 30.0,
    blur: 24,
    bezel: 14.0,
    refraction: 15.0,
    tint: 0.16,
    base_dark: 0.12,
    frost_dark: 0.28,
    frost_light: 0.08,
    saturation: 1.4,
    edge_sharp: 0.75,
    shadow: 22,
    shadow_alpha: 0.30,
};

pub const TOOLTIP: GlassStyle = GlassStyle {
    radius: 16.0,
    blur: 10,
    bezel: 9.0,
    refraction: 8.0,
    tint: 0.14,
    base_dark: 0.14,
    frost_dark: 0.28,
    frost_light: 0.08,
    saturation: 1.4,
    edge_sharp: 0.6,
    shadow: 10,
    shadow_alpha: 0.25,
};

impl GlassStyle {
    /// How far outside the panel the backdrop must already be composed
    /// before the glass can be computed (blur kernel reach + refraction
    /// reach), which is also how far its own shadow extends.
    pub fn pad(&self) -> i32 {
        (self.blur + self.refraction as i32 + 2).max(self.reach())
    }

    /// How far outside the panel it actually changes pixels (its shadow).
    pub fn reach(&self) -> i32 {
        self.shadow + self.shadow / 3 + 2
    }
}

/// Refraction strength against normalised depth into the bezel (index 0
/// at the rim, 64 at the bezel's inner edge, where the glass is flat).
fn refraction_profile() -> [f32; 65] {
    const N: f32 = 1.5; // index of refraction (glass)
    let mut lut = [0.0f32; 65];
    let mut max = 0.0f32;
    for (i, out) in lut.iter_mut().enumerate() {
        let t = i as f32 / 64.0;
        // Surface height follows a squircle profile z = (1 - (1-t)^4)^(1/4),
        // flat in the middle and steep at the rim; its slope is dz/dt.
        let u = 1.0 - t;
        let u3 = u * u * u;
        let base = (1.0 - u3 * u).max(1e-4);
        let slope = 0.55 * u3 / (sqrt(base) * sqrt(sqrt(base)));
        // A vertical ray meeting a surface tilted by atan(slope): angle of
        // incidence theta1 (tan = slope), refracted angle theta2 from
        // Snell's law sin(theta2) = sin(theta1) / n. The ray leaves bent by
        // (theta1 - theta2); its sideways travel through the glass is
        // proportional to tan of that.
        let sin1 = slope / sqrt(1.0 + slope * slope);
        let sin2 = sin1 / N;
        let tan2 = sin2 / sqrt(1.0 - sin2 * sin2);
        let bend = (slope - tan2) / (1.0 + slope * tan2);
        *out = bend;
        max = max.max(bend);
    }
    for v in lut.iter_mut() {
        *v /= max;
    }
    lut
}

/// Everything about a panel that depends only on its size and style.
#[derive(Default)]
pub struct Shape {
    key: (i32, i32, u32, u32, u32, i32),
    w: i32,
    h: i32,
    /// Coverage, 0..=255.
    pub cov: Vec<u8>,
    /// Backdrop sample offset, (dx, dy) per pixel in 1/16 px.
    disp: Vec<i16>,
    /// Additive white (rim light + glint), 0..=255.
    light: Vec<u8>,
    /// Closeness to the rim inside the bezel, 0 (flat) ..= 255 (rim).
    edge: Vec<u8>,
    /// Darkening on the shadowed side of the rim, 0..=255.
    shade: Vec<u8>,
    /// Drop shadow opacity over a box `shadow_margin` larger on each side.
    shadow: Vec<u8>,
    shadow_margin: i32,
}

impl Shape {
    pub fn ensure(&mut self, w: i32, h: i32, s: &GlassStyle) {
        let key = (w, h, s.radius.to_bits(), s.bezel.to_bits(), s.refraction.to_bits(), s.shadow);
        if key == self.key && !self.cov.is_empty() {
            return;
        }
        self.key = key;
        self.w = w;
        self.h = h;
        let n = (w.max(0) * h.max(0)) as usize;
        self.cov.clear();
        self.cov.resize(n, 0);
        self.disp.clear();
        self.disp.resize(n * 2, 0);
        self.light.clear();
        self.light.resize(n, 0);
        self.edge.clear();
        self.edge.resize(n, 0);
        self.shade.clear();
        self.shade.resize(n, 0);

        let lut = refraction_profile();
        let hw = w as f32 * 0.5;
        let hh = h as f32 * 0.5;
        let r = s.radius.min(hw).min(hh);
        // Unit vector pointing at the light (up and to the left).
        let (lx, ly) = (-0.53f32, -0.85f32);
        let interior = s.bezel.max(r) + 2.0;

        for y in 0..h {
            let py = y as f32 + 0.5 - hh;
            let vertical = 1.0 - (y as f32 / h as f32);
            let rim_strength = 0.16 + 0.70 * vertical * vertical;
            for x in 0..w {
                let px = x as f32 + 0.5 - hw;
                let i = (y * w + x) as usize;
                // Fast path: comfortably inside both the corners and the
                // bezel band, the glass is flat -- full coverage, no bend.
                let ax = px.abs();
                let ay = py.abs();
                if (ax < hw - interior && ay < hh - s.bezel - 2.0) || (ay < hh - interior && ax < hw - s.bezel - 2.0) {
                    self.cov[i] = 255;
                    continue;
                }

                let d = squircle_sdf(px, py, hw, hh, r);
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                self.cov[i] = (cov * 255.0) as u8;

                // Outward normal: the SDF's gradient.
                let qx = ax - (hw - r);
                let qy = ay - (hh - r);
                let (mut nx, mut ny) = if qx > 0.0 && qy > 0.0 {
                    let (gx, gy) = (qx * qx * qx, qy * qy * qy);
                    let l = sqrt(gx * gx + gy * gy).max(1e-6);
                    (gx / l, gy / l)
                } else if qx > qy {
                    (1.0, 0.0)
                } else {
                    (0.0, 1.0)
                };
                if px < 0.0 {
                    nx = -nx;
                }
                if py < 0.0 {
                    ny = -ny;
                }

                let depth = (-d).max(0.0);
                let mut light = 0.0f32;
                let mut shade = 0.0f32;
                // 1px interior border with the top-bright gradient.
                light += (1.0 - depth / 1.6).clamp(0.0, 1.0) * rim_strength;
                // A soft inner glow along the rim, so the edge reads as a
                // thick, curved lip of glass all the way round.
                light += (1.0 - depth / 5.0).clamp(0.0, 1.0) * 0.06;

                if depth < s.bezel {
                    let t = depth / s.bezel;
                    let fi = t * 64.0;
                    let i0 = (fi as usize).min(63);
                    let frac = fi - i0 as f32;
                    let bend = lut[i0] + (lut[i0 + 1] - lut[i0]) * frac;
                    let mag = bend * s.refraction * 16.0;
                    self.disp[i * 2] = (-nx * mag) as i16;
                    self.disp[i * 2 + 1] = (-ny * mag) as i16;
                    self.edge[i] = ((1.0 - t) * 255.0) as u8;

                    // Specular glint where the curved rim faces the light,
                    // and a fainter one on the opposite side.
                    let k = (1.0 - t) * (1.0 - t);
                    let dot = nx * lx + ny * ly;
                    let front = dot.max(0.0);
                    let back = (-dot).max(0.0);
                    light += (front * front * front) * k * 0.42 + (back * back * back) * k * 0.20;
                    // The far side of the lip, facing away from the light,
                    // reads a touch darker -- together with the glint this
                    // gives the rim its rounded, thick-glass look.
                    shade = back * k * 0.16 + k * k * 0.06;
                }
                self.light[i] = (light.clamp(0.0, 1.0) * 255.0) as u8;
                self.shade[i] = (shade.clamp(0.0, 1.0) * 255.0) as u8;
            }
        }

        // Drop shadow: the same SDF pushed down a little and blurred
        // outwards, kept only where the panel itself doesn't cover.
        self.shadow_margin = s.shadow;
        self.shadow.clear();
        if s.shadow > 0 {
            let m = s.shadow;
            let (sw, sh) = (w + 2 * m, h + 2 * m);
            self.shadow.resize((sw * sh) as usize, 0);
            let drop = m as f32 / 3.0;
            for y in 0..sh {
                let py = (y - m) as f32 + 0.5 - hh;
                for x in 0..sw {
                    let px = (x - m) as f32 + 0.5 - hw;
                    let own = (0.5 - squircle_sdf(px, py, hw, hh, r)).clamp(0.0, 1.0);
                    if own >= 1.0 {
                        continue;
                    }
                    let ds = squircle_sdf(px, py - drop, hw, hh, r);
                    let a = s.shadow_alpha * (1.0 - smoothstep(-(m as f32) * 0.35, m as f32, ds)) * (1.0 - own);
                    self.shadow[(y * sw + x) as usize] = (a * 255.0) as u8;
                }
            }
        }
    }

    /// Whether panel-local point `(x, y)` is inside the shape -- the same
    /// SDF the panel is drawn with, so a click just outside a rounded
    /// corner falls through to whatever is underneath.
    pub fn hit(&self, x: i32, y: i32) -> bool {
        if self.cov.is_empty() {
            return true; // Not drawn yet; the caller's bounds check stands.
        }
        x >= 0 && y >= 0 && x < self.w && y < self.h && self.cov[(y * self.w + x) as usize] >= 128
    }
}

/// Reusable scratch planes for the blur, shared by every panel (one big
/// allocation reused forever beats many short-lived ones on this kernel's
/// non-coalescing heap).
pub struct Scratch {
    planes: [Vec<u8>; 4],
    /// The graded low-resolution field, packed RGB.
    low: Vec<u32>,
    /// One vertically-interpolated row of `low`.
    row: Vec<u32>,
}

impl Scratch {
    pub fn new(capacity: usize) -> Self {
        let mk = || Vec::with_capacity(capacity);
        Scratch { planes: [mk(), mk(), mk(), mk()], low: Vec::with_capacity(capacity), row: Vec::with_capacity(4096) }
    }
}

fn box_blur(src: &mut [u8], tmp: &mut [u8], w: usize, h: usize, r: usize) {
    // Multiply-and-shift instead of dividing by the window size per pixel.
    let inv = (1u32 << 16) / (2 * r as u32 + 1) + 1;
    // Horizontal: src -> tmp.
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        let out = &mut tmp[y * w..(y + 1) * w];
        let at = |i: isize| row[i.clamp(0, w as isize - 1) as usize] as u32;
        let mut sum: u32 = (-(r as isize)..=r as isize).map(at).sum();
        for x in 0..w {
            out[x] = ((sum * inv) >> 16) as u8;
            sum += at(x as isize + r as isize + 1);
            sum -= at(x as isize - r as isize);
        }
    }
    // Vertical: tmp -> src.
    for x in 0..w {
        let at = |i: isize| tmp[i.clamp(0, h as isize - 1) as usize * w + x] as u32;
        let mut sum: u32 = (-(r as isize)..=r as isize).map(at).sum();
        for y in 0..h {
            src[y * w + x] = ((sum * inv) >> 16) as u8;
            sum += at(y as isize + r as isize + 1);
            sum -= at(y as isize - r as isize);
        }
    }
}

/// One panel's glass: its shape plus its cached, backdrop-dependent colour.
#[derive(Default)]
pub struct Glass {
    pub shape: Shape,
    cache: Vec<u32>,
    cache_rect: Rect,
    valid: bool,
}

impl Glass {
    pub fn invalidate(&mut self) {
        self.valid = false;
    }

    pub fn needs_compute(&self, rect: Rect) -> bool {
        !self.valid || self.cache_rect != rect
    }

    /// Composes this panel at `rect` onto `bb` within `clip`, at `opacity`.
    /// If the cached glass is stale it's recomputed from `bb` first --
    /// which must already hold everything beneath the panel over
    /// `rect.expand(style.pad())`.
    pub fn render(&mut self, bb: &mut Surface, rect: Rect, s: &GlassStyle, clip: Rect, opacity: u8, scratch: &mut Scratch) {
        if rect.is_empty() {
            return;
        }
        self.shape.ensure(rect.w, rect.h, s);
        if self.needs_compute(rect) {
            self.compute(bb, rect, s, scratch);
        }
        let op = opacity as u32;
        let bw = bb.w;

        let c = rect.intersect(&clip).intersect(&bb.bounds());
        for y in c.y..c.bottom() {
            let lrow = ((y - rect.y) * rect.w) as usize;
            for x in c.x..c.right() {
                let li = lrow + (x - rect.x) as usize;
                let cov = self.shape.cov[li] as u32;
                if cov == 0 {
                    continue;
                }
                let i = (y * bw + x) as usize;
                bb.px[i] = blend(bb.px[i], self.cache[li], (cov * op * 257 + 0x8000) >> 16);
            }
        }

        let m = self.shape.shadow_margin;
        if m > 0 && !self.shape.shadow.is_empty() {
            let srect = rect.expand(m);
            let c = srect.intersect(&clip).intersect(&bb.bounds());
            for y in c.y..c.bottom() {
                let lrow = ((y - srect.y) * srect.w) as usize;
                for x in c.x..c.right() {
                    let a = self.shape.shadow[lrow + (x - srect.x) as usize] as u32;
                    if a == 0 {
                        continue;
                    }
                    let i = (y * bw + x) as usize;
                    bb.px[i] = blend(bb.px[i], 0, (a * op * 257 + 0x8000) >> 16);
                }
            }
        }
    }

    // Everything per-pixel below is integer fixed point on purpose: under
    // QEMU's software CPU every SSE float operation is an emulated helper
    // call, and the float version of this loop was an order of magnitude
    // slower. For the same reason the colour grade runs once per low-res
    // cell, not once per screen pixel: the flat interior of a panel is a
    // smooth blurred field, so it's graded at low resolution and simply
    // upsampled. Only the bezel -- where the backdrop is refracted and
    // sampled sharp -- is graded per pixel.
    fn compute(&mut self, bb: &Surface, rect: Rect, s: &GlassStyle, scratch: &mut Scratch) {
        self.cache_rect = rect;
        self.valid = true;
        let n = (rect.w * rect.h) as usize;
        self.cache.clear();
        self.cache.resize(n, 0);

        let region = rect.expand(s.blur + s.refraction as i32 + 2).intersect(&bb.bounds());
        if region.is_empty() {
            return;
        }
        let shift: i32 = if s.blur >= 24 { 2 } else if s.blur >= 8 { 1 } else { 0 };
        let f = 1i32 << shift;
        let lw = ((region.w + f - 1) >> shift) as usize;
        let lh = ((region.h + f - 1) >> shift) as usize;
        let Scratch { planes: [pr, pg, pb, tmp], low, row } = scratch;
        for p in [&mut *pr, &mut *pg, &mut *pb, &mut *tmp] {
            p.clear();
            p.resize(lw * lh, 0);
        }

        // Downsample the backdrop by f x f box averaging.
        let (mut sum_r, mut sum_g, mut sum_b) = (0u64, 0u64, 0u64);
        for ly in 0..lh {
            let y0 = region.y + ((ly as i32) << shift);
            let y1 = (y0 + f).min(region.bottom());
            for lx in 0..lw {
                let x0 = region.x + ((lx as i32) << shift);
                let x1 = (x0 + f).min(region.right());
                let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
                for y in y0..y1 {
                    let px = &bb.px[(y * bb.w + x0) as usize..(y * bb.w + x1) as usize];
                    for &p in px {
                        r += (p >> 16) & 0xff;
                        g += (p >> 8) & 0xff;
                        b += p & 0xff;
                    }
                }
                let cnt = ((y1 - y0) * (x1 - x0)) as u32;
                let (r, g, b) = if cnt == (f * f) as u32 {
                    (r >> (2 * shift), g >> (2 * shift), b >> (2 * shift))
                } else {
                    (r / cnt, g / cnt, b / cnt)
                };
                let i = ly * lw + lx;
                pr[i] = r as u8;
                pg[i] = g as u8;
                pb[i] = b as u8;
                sum_r += r as u64;
                sum_g += g as u64;
                sum_b += b as u64;
            }
        }
        let cells = (lw * lh) as u64;
        let mean = ((sum_r / cells) as i32, (sum_g / cells) as i32, (sum_b / cells) as i32);

        // Three box passes ~ a Gaussian with sigma ~ blur / 2.
        let r = (((s.blur >> shift) + 1) / 2).max(1) as usize;
        for _ in 0..3 {
            box_blur(pr, tmp, lw, lh, r);
            box_blur(pg, tmp, lw, lh, r);
            box_blur(pb, tmp, lw, lh, r);
        }

        // Adaptive tint colour: the backdrop's average, with its saturation
        // pushed up so the glass picks up the hue rather than going grey.
        let mean_lum = (54 * mean.0 + 183 * mean.1 + 19 * mean.2) >> 8;
        let boost = |c: i32| (mean_lum + (((c - mean_lum) * 410) >> 8)).clamp(0, 255);
        let tint = (boost(mean.0), boost(mean.1), boost(mean.2));

        // Style constants in 1/256 units.
        let k256 = |v: f32| (v * 256.0) as i32;
        let (tint_k, sat, base_dark, frost_dark, frost_light, edge_sharp) =
            (k256(s.tint), k256(s.saturation), k256(s.base_dark), k256(s.frost_dark), k256(s.frost_light), k256(s.edge_sharp));

        // The glass's colour response to whatever is behind it.
        let grade = |mut r: i32, mut g: i32, mut b: i32| -> (i32, i32, i32) {
            // Vibrancy: push the backdrop's saturation up.
            let gray = (54 * r + 183 * g + 19 * b) >> 8;
            r = (gray + (((r - gray) * sat) >> 8)).clamp(0, 255);
            g = (gray + (((g - gray) * sat) >> 8)).clamp(0, 255);
            b = (gray + (((b - gray) * sat) >> 8)).clamp(0, 255);
            // Inject the backdrop's average hue.
            r += ((tint.0 - r) * tint_k) >> 8;
            g += ((tint.1 - g) * tint_k) >> 8;
            b += ((tint.2 - b) * tint_k) >> 8;
            // Contrast-preserving frost: darken over light backdrops, lift
            // over dark ones, judged half locally, half globally.
            let lum = (((54 * r + 183 * g + 19 * b) >> 8) + mean_lum) >> 1;
            let dark = base_dark + (((lum - 77) * 307) >> 8).clamp(0, frost_dark);
            let lift = (((77 - lum) * 205) >> 8).clamp(0, frost_light);
            let keep = 256 - dark;
            r = (r * keep) >> 8;
            g = (g * keep) >> 8;
            b = (b * keep) >> 8;
            r += ((255 - r) * lift) >> 8;
            g += ((255 - g) * lift) >> 8;
            b += ((255 - b) * lift) >> 8;
            (r, g, b)
        };

        // Grade the blurred field once, at low resolution, packed RGB.
        low.clear();
        low.extend((0..lw * lh).map(|i| {
            let (r, g, b) = grade(pr[i] as i32, pg[i] as i32, pb[i] as i32);
            ((r as u32) << 16) | ((g as u32) << 8) | b as u32
        }));
        row.clear();
        row.resize(lw, 0);

        let (max_u, max_v) = (((lw - 1) as i32) << 8, ((lh - 1) as i32) << 8);
        // Screen position in 1/16 px -> low-res coordinate in 1/256 px.
        let to_u = |p16: i32| (((p16 - (region.x << 4)) << (4 - shift)) - 128).clamp(0, max_u);
        let to_v = |p16: i32| (((p16 - (region.y << 4)) << (4 - shift)) - 128).clamp(0, max_v);
        // Bilinear sample of the graded field (one packed pixel).
        let sample_low = |u: i32, v: i32| -> u32 {
            let (u0, v0) = ((u >> 8) as usize, (v >> 8) as usize);
            let (u1, v1) = ((u0 + 1).min(lw - 1), (v0 + 1).min(lh - 1));
            let top = blend(low[v0 * lw + u0], low[v0 * lw + u1], (u & 255) as u32);
            let bot = blend(low[v1 * lw + u0], low[v1 * lw + u1], (u & 255) as u32);
            blend(top, bot, (v & 255) as u32)
        };
        // Full-resolution, unblurred backdrop sample at a screen position
        // in 1/16 px -- what the rim refracts, so the lens distortion is
        // actually visible.
        let sharp = |x16: i32, y16: i32| -> u32 {
            let u = (x16 - 8).clamp(region.x << 4, (region.right() - 1) << 4);
            let v = (y16 - 8).clamp(region.y << 4, (region.bottom() - 1) << 4);
            let (x0, y0) = (u >> 4, v >> 4);
            let x1 = (x0 + 1).min(region.right() - 1);
            let y1 = (y0 + 1).min(region.bottom() - 1);
            let at = |x: i32, y: i32| bb.px[(y * bb.w + x) as usize];
            let (tu, tv) = (((u & 15) * 17) as u32, ((v & 15) * 17) as u32);
            blend(blend(at(x0, y0), at(x1, y0), tu), blend(at(x0, y1), at(x1, y1), tu), tv)
        };
        let ch = |p: u32, sh: u32| ((p >> sh) & 0xff) as i32;

        let vis = rect.intersect(&bb.bounds());
        for y in vis.y..vis.bottom() {
            let lrow = ((y - rect.y) * rect.w) as usize;
            let cy16 = (y << 4) + 8;
            // Vertical half of the bilinear upsample, once per row.
            let v = to_v(cy16);
            let (v0, v1) = ((v >> 8) as usize, ((v >> 8) as usize + 1).min(lh - 1));
            for (lx, out) in row.iter_mut().enumerate() {
                *out = blend(low[v0 * lw + lx], low[v1 * lw + lx], (v & 255) as u32);
            }
            for x in vis.x..vis.right() {
                let li = lrow + (x - rect.x) as usize;
                if self.shape.cov[li] == 0 {
                    continue;
                }
                let cx16 = (x << 4) + 8;
                let dx = self.shape.disp[li * 2] as i32;
                let dy = self.shape.disp[li * 2 + 1] as i32;
                let (mut r, mut g, mut b);
                if dx == 0 && dy == 0 {
                    let u = to_u(cx16);
                    let u0 = (u >> 8) as usize;
                    let p = blend(row[u0], row[(u0 + 1).min(lw - 1)], (u & 255) as u32);
                    (r, g, b) = (ch(p, 16), ch(p, 8), ch(p, 0));
                } else {
                    // Dispersion: red bends a little more than green, blue
                    // a little less.
                    let (rx, ry) = (cx16 + ((dx * 302) >> 8), cy16 + ((dy * 302) >> 8));
                    let (gx, gy) = (cx16 + dx, cy16 + dy);
                    let (bx, by) = (cx16 + ((dx * 210) >> 8), cy16 + ((dy * 210) >> 8));
                    let br = ch(sample_low(to_u(rx), to_v(ry)), 16);
                    let bg = ch(sample_low(to_u(gx), to_v(gy)), 8);
                    let bl = ch(sample_low(to_u(bx), to_v(by)), 0);
                    // Toward the rim, less frost: the bent backdrop shows
                    // through sharp, like looking through a lens edge.
                    let e = self.shape.edge[li] as i32;
                    let k = (edge_sharp * e * e) >> 16;
                    let (sr, sg, sb) = grade(ch(sharp(rx, ry), 16), ch(sharp(gx, gy), 8), ch(sharp(bx, by), 0));
                    r = br + (((sr - br) * k) >> 8);
                    g = bg + (((sg - bg) * k) >> 8);
                    b = bl + (((sb - bl) * k) >> 8);
                }

                let shade = self.shape.shade[li] as i32;
                if shade != 0 {
                    r = (r * (256 - shade)) >> 8;
                    g = (g * (256 - shade)) >> 8;
                    b = (b * (256 - shade)) >> 8;
                }
                let light = self.shape.light[li] as i32;
                if light != 0 {
                    r += ((255 - r) * light) >> 8;
                    g += ((255 - g) * light) >> 8;
                    b += ((255 - b) * light) >> 8;
                }

                let noise = (hash2(x, y) & 15) as i32 - 7;
                let r = (r + noise).clamp(0, 255) as u32;
                let g = (g + noise).clamp(0, 255) as u32;
                let b = (b + noise).clamp(0, 255) as u32;
                self.cache[li] = (r << 16) | (g << 8) | b;
            }
        }
    }
}
