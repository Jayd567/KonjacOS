//! The mouse pointer's artwork: the whole "Minimalistic Modern Cursor Set"
//! by Dante Berlin (Creative Commons Attribution,
//! `material-design-best-edition-by`), baked into the kernel binary.
//!
//! The pack is Windows `.cur` files (32x32, 32-bit, real per-pixel alpha)
//! plus two animated `.ani` files (RIFF containers of `.cur` frames).
//! Rather than parse either format in the kernel, `tools/gen_desktop_assets.py`
//! decodes them all offline into `assets/cursors.kcur`:
//!
//! "KCUR", u16 count, then per cursor (in [`Shape`] order) a 16-byte record
//! `{u16 w, u16 h, i16 hotspot x, i16 hotspot y, u16 frames, u16 ticks per
//! frame, u32 offset}`, then each cursor's frames as top-down RGBA8.

/// Which pointer to show, named after what it means rather than its file
/// (`arrow.cur`, `text.cur`, ...). Order matches `CURSORS` in the script.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    Arrow,
    /// I-beam, over text.
    Text,
    /// Pointing hand, over links and launchable things.
    Hand,
    /// Four-way arrow, while moving a window or icon.
    Move,
    /// Resizing: up/down, left/right, and the two diagonals.
    SizeNS,
    SizeWE,
    SizeNWSE,
    SizeNESW,
    /// Not allowed: disabled menu items, drops that can't happen.
    No,
    /// Crosshair, for rubber-band selection.
    Cross,
    /// Arrow with a question mark, over things that explain themselves.
    Help,
    /// Pen, over a drawing canvas.
    Pen,
    /// Arrow with a person, over the user account.
    Person,
    /// Arrow with a location pin, where a drop pins something.
    Pin,
    /// Upward arrow, over "go up a level".
    Up,
    /// Animated spinner: this thing is busy.
    Busy,
    /// Animated arrow-and-spinner: an app is starting, but you can still
    /// point at things.
    Working,
}

static CURSORS: &[u8] = include_bytes!("../assets/cursors.kcur");

/// One cursor's metadata and pixels.
pub struct Image {
    pub w: i32,
    pub h: i32,
    pub hot_x: i32,
    pub hot_y: i32,
    frames: u16,
    ticks: u16,
    data: &'static [u8],
}

impl Image {
    /// Which frame is on screen at timer tick `now` (always 0 for a
    /// static cursor). The desktop redraws the pointer when this changes.
    pub fn frame_at(&self, now: u64) -> u16 {
        if self.frames <= 1 || self.ticks == 0 {
            return 0;
        }
        ((now / self.ticks as u64) % self.frames as u64) as u16
    }

    /// Frame `frame`'s pixels: `w * h` top-down RGBA8.
    pub fn pixels(&self, frame: u16) -> &'static [u8] {
        let size = (self.w * self.h * 4) as usize;
        let start = (frame.min(self.frames - 1)) as usize * size;
        &self.data[start..start + size]
    }
}

fn u16_at(i: usize) -> u16 {
    u16::from_le_bytes([CURSORS[i], CURSORS[i + 1]])
}

pub fn image(shape: Shape) -> Image {
    let rec = 6 + shape as usize * 16;
    let off = u32::from_le_bytes([CURSORS[rec + 12], CURSORS[rec + 13], CURSORS[rec + 14], CURSORS[rec + 15]]) as usize;
    Image {
        w: u16_at(rec) as i32,
        h: u16_at(rec + 2) as i32,
        hot_x: u16_at(rec + 4) as i16 as i32,
        hot_y: u16_at(rec + 6) as i16 as i32,
        frames: u16_at(rec + 8).max(1),
        ticks: u16_at(rec + 10),
        data: &CURSORS[off..],
    }
}
