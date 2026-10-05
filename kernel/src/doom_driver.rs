//! Rust-side glue for `csrc/doom/doomgeneric_konjac.c`, the platform
//! driver implementing doomgeneric's six-function porting API
//! (`DG_Init`/`DG_DrawFrame`/`DG_SleepMs`/`DG_GetTicksMs`/`DG_GetKey`/
//! `DG_SetWindowTitle`). Kept in its own module (rather than folded into
//! `keyboard.rs`/`timer.rs`) since none of this is anything those modules
//! would otherwise have a reason to expose -- it's DOOM-specific plumbing
//! on top of general-purpose facilities they already provide.
//!
//! DOOM doesn't touch the screen itself: each finished frame is copied
//! into [`FRAME`], and the desktop (see `ui/desktop.rs`) composites it
//! into DOOM's window, with the same glass chrome every other window has.
//! The desktop opens that window as soon as [`running_task`] reports a
//! DOOM task, and closing it kills the task.

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::keyboard;
use crate::sync::SpinLock;
use crate::task;
use crate::timer;

/// DOOM's fixed output size (doomgeneric always upscales into a 640x400
/// `DG_ScreenBuffer`, see `i_video.c`).
pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 400;

/// The most recent complete frame, `0x00RRGGBB`, `WIDTH * HEIGHT`.
pub static FRAME: SpinLock<Vec<u32>> = SpinLock::new(Vec::new());
/// Bumped after every frame copied into [`FRAME`].
pub static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);
/// The running DOOM task's ID, or 0 if DOOM isn't running.
static TASK_ID: AtomicU64 = AtomicU64::new(0);

/// The DOOM task's ID, if it is (still) running.
pub fn running_task() -> Option<u64> {
    let id = TASK_ID.load(Ordering::Relaxed);
    if id == 0 {
        return None;
    }
    if task::list().iter().any(|&(tid, _, state, ..)| tid == id && state != task::TaskState::Terminated) {
        Some(id)
    } else {
        TASK_ID.store(0, Ordering::Relaxed);
        None
    }
}

pub fn set_running_task(id: u64) {
    FRAME_SEQ.store(0, Ordering::Relaxed);
    TASK_ID.store(id, Ordering::Relaxed);
}

/// Copies doomgeneric's 640x400 XRGB8888 screen buffer into [`FRAME`].
///
/// # Safety
/// `buf` must point at `width * height` valid `u32` pixels in `0x00RRGGBB`
/// order (doomgeneric's own convention for a 32bpp `pixel_t`, confirmed by
/// reading `i_video.c`'s `cmap_to_fb`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn konjac_doom_blit(buf: *const u32, width: u32, height: u32) {
    let (w, h) = ((width as usize).min(WIDTH), (height as usize).min(HEIGHT));
    let mut frame = FRAME.lock();
    if frame.len() != WIDTH * HEIGHT {
        frame.clear();
        frame.resize(WIDTH * HEIGHT, 0);
    }
    for y in 0..h {
        // Safety: rows below `h` and columns below `w` are within the
        // `width * height` pixels the caller promised are valid.
        let src = unsafe { core::slice::from_raw_parts(buf.add(y * width as usize), w) };
        frame[y * WIDTH..y * WIDTH + w].copy_from_slice(src);
    }
    drop(frame);
    FRAME_SEQ.fetch_add(1, Ordering::Release);
}

/// Milliseconds since `timer::init()`, derived from the PIT's 100 Hz tick
/// counter (10ms resolution -- plenty for DOOM's own ~35 Hz internal
/// tic rate).
#[unsafe(no_mangle)]
pub extern "C" fn konjac_doom_ticks_ms() -> u32 {
    (timer::ticks() * 1000 / timer::HZ as u64) as u32
}

/// Waits at least `ms` milliseconds. Whole ticks are slept properly (other
/// tasks, the desktop included, get the CPU); a sub-tick remainder is
/// spent yielding.
#[unsafe(no_mangle)]
pub extern "C" fn konjac_doom_sleep_ms(ms: u32) {
    let start = konjac_doom_ticks_ms();
    let tick_ms = 1000 / timer::HZ;
    while konjac_doom_ticks_ms().wrapping_sub(start) < ms {
        if ms - konjac_doom_ticks_ms().wrapping_sub(start) >= tick_ms {
            task::sleep_ticks(1);
        } else {
            task::yield_now();
        }
    }
}

/// # Safety
/// `pressed`/`key` must be valid, non-null, writable pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn konjac_doom_get_key(pressed: *mut i32, key: *mut u8) -> i32 {
    match keyboard::read_doom_event() {
        Some((was_pressed, doomkey)) => {
            unsafe {
                *pressed = was_pressed as i32;
                *key = doomkey;
            }
            1
        }
        None => 0,
    }
}
