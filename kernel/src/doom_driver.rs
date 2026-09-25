//! Rust-side glue for `csrc/doom/doomgeneric_konjac.c`, the platform
//! driver implementing doomgeneric's six-function porting API
//! (`DG_Init`/`DG_DrawFrame`/`DG_SleepMs`/`DG_GetTicksMs`/`DG_GetKey`/
//! `DG_SetWindowTitle`). Kept in its own module (rather than folded into
//! `keyboard.rs`/`timer.rs`/`framebuffer.rs` directly) since none of this
//! is anything those modules would otherwise have a reason to expose --
//! it's DOOM-specific plumbing on top of general-purpose facilities they
//! already provide.
//!
//! DOOM is also this kernel's first *automatically windowed* program (see
//! [`doom_window_init`]): it gets a real `wm.rs`-style title bar and a
//! real, clickable X close button the instant it starts, without anyone
//! having to run the `gui` desktop first -- the desktop demo and DOOM's
//! own window are two different call sites sharing one drawing module
//! now, not two different looks.

extern crate alloc;

use alloc::vec::Vec;

use crate::console;
use crate::keyboard;
use crate::mouse;
use crate::task;
use crate::timer;
use crate::wm;

/// DOOM's fixed internal screen size (see [`konjac_doom_blit`]'s doc
/// comment for why this is always exactly this size). What
/// [`doom_window_init`] sizes the window's client area to.
const DOOM_W: i32 = 640;
const DOOM_H: i32 = 400;

/// This DOOM session's on-screen window -- opened once by
/// [`doom_window_init`] right before the render loop starts, and read by
/// every subsequent [`konjac_doom_blit`] call to know where the client
/// area actually is and whether the close button just got clicked. Fixed
/// for the task's whole lifetime: unlike `wm.rs`'s own demo windows,
/// nothing here drags it around -- it opens centered and stays there.
/// `None` until `doom_window_init` runs, so a stray `konjac_doom_blit`
/// call before that (shouldn't happen, but nothing panics if it does)
/// just falls back to the old whole-screen-centered blit this driver
/// always used before item 28.
struct DoomWindow {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// The screen exactly as it looked the instant before this window's
    /// chrome was first drawn -- what a close-button click restores,
    /// the same "freeze it, draw over it, put it back" pattern `wm.rs`'s
    /// own demo loop already uses for its background.
    background: Vec<u8>,
    /// Edge-detects a fresh left-click the same way `wm.rs`'s own demo
    /// loop does, so holding the button down over the close button for
    /// more than one frame doesn't try to close (and exit the task)
    /// more than once.
    prev_left_down: bool,
}

static mut DOOM_WINDOW: Option<DoomWindow> = None;

/// Opens this DOOM session's window and draws its initial chrome -- the
/// automatic equivalent of what a person would otherwise get by running
/// `gui` and dragging a window into place, minus having to run `gui` at
/// all or do any dragging. Called once, by
/// `commands.rs::doom_task_entry`, right after `doomgeneric_Create`
/// (DOOM's own startup -- WAD loading, `DG_Init`, ...) but before the
/// render loop begins, so the window frame is already up and visible
/// before the very first real game frame lands inside it.
pub fn doom_window_init() {
    let mut console = console::CONSOLE.lock();
    let Some(canvas) = console.canvas_mut() else { return };

    let background = canvas.snapshot();
    let (sw, sh) = (canvas.width() as i32, canvas.height() as i32);
    let w = DOOM_W.min(sw);
    let h = (DOOM_H + wm::TITLE_BAR_H).min(sh);
    let x = ((sw - w) / 2).max(0);
    let y = ((sh - h) / 2).max(0);

    wm::draw_chrome(canvas, x, y, w, h, "DOOM", false);
    // The client area starts out black -- DOOM's own first real frame
    // (moments away, once the render loop starts calling
    // konjac_doom_blit) overwrites it before anyone would notice either
    // way, but leaving raw framebuffer garbage there until then would
    // look like a bug rather than a window that just hasn't rendered
    // its first frame yet.
    let client_h = (h - wm::TITLE_BAR_H).max(0);
    canvas.fill_rect(x as u64, (y + wm::TITLE_BAR_H) as u64, w as u64, client_h as u64, 0, 0, 0);

    unsafe {
        DOOM_WINDOW = Some(DoomWindow { x, y, w, h, background, prev_left_down: false });
    }
}

/// Blits doomgeneric's 640x400 XRGB8888 screen buffer (`DG_ScreenBuffer`,
/// always this fixed size regardless of the actual `SCREENWIDTH`/
/// `SCREENHEIGHT` DOOM renders at -- see `i_video.c`'s `cmap_to_fb`/
/// `fb_scaling` dance, which upscales into it) into this session's window
/// (see [`doom_window_init`]), then redraws that window's chrome (close-
/// button hover feedback included) and the real desktop cursor on top --
/// the same per-frame "redraw everything that could have changed" model
/// `wm.rs`'s own loop uses, affordable here for the same reason it is
/// there: this is a small, cheap-to-redraw title bar and border, not the
/// expensive part of the frame.
///
/// Also where a close-button click is actually acted on: a fresh
/// left-click (edge-detected against last frame, so holding the button
/// doesn't retrigger every frame) inside the close button restores the
/// screen to how it looked before this window ever opened and calls
/// [`task::exit_current`] -- never returns, same as any other real exit
/// path a running C program can take here (see `libc_shim.rs`'s own
/// `exit`/`abort`), abandoning this task's kernel stack mid-`DG_DrawFrame`
/// exactly the way those do; safe for the same reason theirs is; nothing
/// downstream of this call ever resumes.
///
/// If [`DOOM_WINDOW`] hasn't been opened yet (`doom_window_init` never
/// ran), falls back to the old whole-screen-centered blit with no chrome
/// at all -- this driver's original behavior, kept as an honest fallback
/// rather than silently drawing nothing.
///
/// # Safety
/// `buf` must point at `width * height` valid `u32` pixels in `0x00RRGGBB`
/// order (doomgeneric's own convention for a 32bpp `pixel_t`, confirmed by
/// reading `i_video.c`'s `cmap_to_fb`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn konjac_doom_blit(buf: *const u32, width: u32, height: u32) {
    let mut console = console::CONSOLE.lock();
    let Some(canvas) = console.canvas_mut() else { return };

    let (cw, ch) = (canvas.width(), canvas.height());
    let (w, h) = (width as u64, height as u64);

    // `&raw mut` + a manual deref, not `&mut DOOM_WINDOW` directly --
    // same "safe in practice (only this function and doom_window_init
    // ever touch this static, and never reentrantly), but spelled the
    // way the compiler wants a mutable static's address taken" as
    // idt.rs's own `core::ptr::addr_of!` pattern for its static IDT.
    let window: &mut Option<DoomWindow> = unsafe { &mut *(&raw mut DOOM_WINDOW) };
    let (x_off, y_off, draw_w, draw_h) = match window {
        Some(win) => {
            let client_w = (win.w as u64).min(w);
            let client_h = (win.h as i64 - wm::TITLE_BAR_H as i64).max(0) as u64;
            (win.x as u64, (win.y + wm::TITLE_BAR_H) as u64, client_w.min(w), client_h.min(h))
        }
        None => {
            let x_off = if cw > w { (cw - w) / 2 } else { 0 };
            let y_off = if ch > h { (ch - h) / 2 } else { 0 };
            (x_off, y_off, w.min(cw), h.min(ch))
        }
    };

    for y in 0..draw_h {
        for x in 0..draw_w {
            // Safety: `x < width` and `y < height` (both bounded by
            // draw_w/draw_h above), so this stays within the `width *
            // height` pixels the caller promised are valid.
            let pixel = unsafe { *buf.add((y * w as u64 + x) as usize) };
            let r = ((pixel >> 16) & 0xff) as u8;
            let g = ((pixel >> 8) & 0xff) as u8;
            let b = (pixel & 0xff) as u8;
            canvas.put_pixel(x_off + x, y_off + y, r, g, b);
        }
    }

    if let Some(win) = window {
        let (mx, my) = mouse::position();
        let left_down = mouse::left_button_down();
        let hover = wm::point_in_close_button(win.x, win.y, win.w, mx, my);

        wm::draw_chrome(canvas, win.x, win.y, win.w, win.h, "DOOM", hover);
        wm::draw_cursor(canvas, mx, my);

        let fresh_click = left_down && !win.prev_left_down;
        win.prev_left_down = left_down;

        if fresh_click && hover {
            canvas.blit(&win.background);
            drop(console);
            crate::println!("doom: closed from its window's X button.");
            task::exit_current();
        }
    }
}

/// Milliseconds since `timer::init()`, derived from the PIT's 100 Hz tick
/// counter (10ms resolution -- plenty for DOOM's own ~35 Hz internal
/// tic rate).
#[unsafe(no_mangle)]
pub extern "C" fn konjac_doom_ticks_ms() -> u32 {
    (timer::ticks() * 1000 / timer::HZ as u64) as u32
}

/// Busy-waits (yielding to the scheduler each spin, so other tasks --
/// notably the shell -- keep running) until at least `ms` milliseconds
/// have passed. There's no real sleep/timer-queue primitive in `task.rs`
/// yet, and DOOM only ever sleeps for single-digit-to-low-double-digit
/// millisecond spans (frame pacing), so this is adequate without needing
/// one.
#[unsafe(no_mangle)]
pub extern "C" fn konjac_doom_sleep_ms(ms: u32) {
    let start = konjac_doom_ticks_ms();
    while konjac_doom_ticks_ms().wrapping_sub(start) < ms {
        task::yield_now();
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
