//! The real cursor-pack asset, baked into the kernel binary.
//!
//! `arrow.cur` (from the "Minimalistic Modern Cursor Set" by Dante Berlin,
//! Creative Commons Attribution, `material-design-best-edition-by`) is a
//! Windows `.cur` file: an ICONDIR + one ICONDIRENTRY pointing at a 32x32,
//! 32-bit-per-pixel DIB with a genuine per-pixel alpha channel (not just a
//! 1-bit AND mask -- this cursor pack has soft anti-aliased edges). Rather
//! than write a `.cur`/ICO parser into the kernel itself for one asset, the
//! pixels were extracted once, offline, with a small Python script (bottom-
//! up BGRA rows -> top-down RGBA, exactly what `blend_pixel` wants) into
//! `assets/cursor_arrow.rgba`, a flat width*height*4 byte buffer with no
//! header at all. `include_bytes!` pulls that straight into `.rodata`.
//!
//! `.ani` (the animated cursors also in that pack, e.g. `busy.ani`,
//! `working.ani`) is a RIFF container holding a whole sequence of `.cur`-
//! like frames plus timing -- deliberately not tackled yet. This gets the
//! static pointer working first; animating it later is a matter of
//! extracting each frame the same way and cycling through them on a timer.

pub const WIDTH: u32 = 32;
pub const HEIGHT: u32 = 32;
/// Where the "hot" point of the cursor is within the bitmap, i.e. the pixel
/// that actually corresponds to the tracked mouse position -- taken
/// straight from `arrow.cur`'s own ICONDIRENTRY hotspot fields.
pub const HOTSPOT_X: i32 = 1;
pub const HOTSPOT_Y: i32 = 1;

/// Flat, top-down, row-major RGBA8 pixels: `WIDTH * HEIGHT * 4` bytes.
static ARROW_RGBA: &[u8] = include_bytes!("../assets/cursor_arrow.rgba");

/// Returns `(r, g, b, a)` for pixel `(x, y)` within the cursor bitmap, or
/// `None` if it's out of bounds. `wm.rs` walks the whole `WIDTH x HEIGHT`
/// box and skips fully-transparent (`a == 0`) pixels itself.
pub fn pixel(x: u32, y: u32) -> Option<(u8, u8, u8, u8)> {
    if x >= WIDTH || y >= HEIGHT {
        return None;
    }
    let idx = ((y * WIDTH + x) * 4) as usize;
    Some((ARROW_RGBA[idx], ARROW_RGBA[idx + 1], ARROW_RGBA[idx + 2], ARROW_RGBA[idx + 3]))
}
