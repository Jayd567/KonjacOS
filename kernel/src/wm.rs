//! A minimal window manager: freezes the current screen as a background,
//! draws a couple of draggable window rectangles and a mouse cursor on top
//! of it, and redraws whenever anything actually changed. Entered via the
//! shell's `gui` command; Esc returns to the text shell.
//!
//! This is deliberately "GUI groundwork", not a real desktop: windows don't
//! host separate running programs yet in *this* loop -- there's nothing
//! here to route input to but this one loop -- and there's no damage
//! tracking beyond "redraw everything from the frozen background snapshot
//! when something moved". The cursor is the real `arrow.cur` bitmap from
//! the cursor pack (see `cursor.rs`), alpha-blended pixel by pixel rather
//! than hard-edged.
//!
//! [`draw_chrome`]/[`close_button_rect`]/[`draw_cursor`] are the actually
//! reusable half of this module: the same title-bar-plus-border-plus-real-
//! X-close-button look this file's own demo windows use, factored out so
//! a *real* running program's own window (see `doom_driver.rs`, the first
//! one) can draw identical chrome around whatever it renders into its own
//! client area, without duplicating the drawing code or inventing a
//! different look. `draw_chrome` deliberately never touches the client
//! rect itself -- the area below the title bar -- so a caller with real
//! content to put there (DOOM's own game frame) can blit directly into it
//! without this module fighting over the same pixels.

extern crate alloc;

use crate::console;
use crate::cursor;
use crate::framebuffer::Canvas;
use crate::keyboard;
use crate::mouse;

struct Window {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    title: &'static str,
    color: (u8, u8, u8),
}

pub const TITLE_BAR_H: i32 = 20;
const TITLE_COLOR: (u8, u8, u8) = (40, 40, 48);
const TITLE_TEXT: (u8, u8, u8) = (230, 230, 230);
const BORDER_COLOR: (u8, u8, u8) = (10, 10, 14);
/// Width of the real, clickable X close button at the right end of every
/// window's title bar -- see [`close_button_rect`].
const CLOSE_BTN_W: i32 = 20;
const CLOSE_BTN_COLOR: (u8, u8, u8) = (170, 50, 50);
const CLOSE_BTN_HOVER_COLOR: (u8, u8, u8) = (214, 74, 74);

impl Window {
    fn contains_title_bar(&self, mx: i32, my: i32) -> bool {
        mx >= self.x && mx < self.x + self.w as i32 && my >= self.y && my < self.y + TITLE_BAR_H
    }
}

/// The close button's hit-test rectangle `(x, y, w, h)` for a window whose
/// title bar spans `[x, x + w)` at vertical position `y` -- always the
/// rightmost `CLOSE_BTN_W` pixels of the title bar, regardless of the
/// window's actual width, so it's reachable the same way in every corner
/// of the screen a window could be dragged to.
pub fn close_button_rect(x: i32, y: i32, w: i32) -> (i32, i32, i32, i32) {
    (x + w - CLOSE_BTN_W, y, CLOSE_BTN_W, TITLE_BAR_H)
}

/// Whether `(mx, my)` falls inside the close button for a window at
/// `(x, y)` with width `w` -- what both this module's own demo loop and
/// `doom_driver.rs`'s per-frame click check use to decide whether a fresh
/// left-click should close the window instead of (or in addition to)
/// whatever else it might do.
pub fn point_in_close_button(x: i32, y: i32, w: i32, mx: i32, my: i32) -> bool {
    let (bx, by, bw, bh) = close_button_rect(x, y, w);
    mx >= bx && mx < bx + bw && my >= by && my < by + bh
}

/// Draws one window's chrome -- title bar (with real text and a real,
/// hoverable X close button) and a 1px border -- around a client rect of
/// `w x h` at `(x, y)`. Never touches the client area itself (everything
/// from `y + TITLE_BAR_H` down to `y + h`): that's the caller's content,
/// whether it's this module's own solid demo-window fill or a real
/// program's actual rendered frame (`doom_driver.rs`).
pub fn draw_chrome(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32, title: &str, close_hover: bool) {
    canvas.fill_rect(x as u64, y as u64, w as u64, TITLE_BAR_H as u64, TITLE_COLOR.0, TITLE_COLOR.1, TITLE_COLOR.2);
    for (i, c) in title.chars().enumerate() {
        canvas.draw_char((x + 4 + i as i32 * 8) as u64, (y + 6) as u64, c, TITLE_TEXT, TITLE_COLOR);
    }

    let (bx, by, bw, bh) = close_button_rect(x, y, w);
    let btn_color = if close_hover { CLOSE_BTN_HOVER_COLOR } else { CLOSE_BTN_COLOR };
    canvas.fill_rect(bx as u64, by as u64, bw as u64, bh as u64, btn_color.0, btn_color.1, btn_color.2);
    // A real "x" glyph, centered in the button -- font.rs's fixed 8x8 cell
    // makes the centering arithmetic this simple.
    canvas.draw_char((bx + (bw - 8) / 2) as u64, (by + (bh - 8) / 2) as u64, 'x', TITLE_TEXT, btn_color);

    canvas.fill_rect(x as u64, y as u64, w as u64, 1, BORDER_COLOR.0, BORDER_COLOR.1, BORDER_COLOR.2);
    canvas.fill_rect(x as u64, (y + h - 1).max(0) as u64, w as u64, 1, BORDER_COLOR.0, BORDER_COLOR.1, BORDER_COLOR.2);
    canvas.fill_rect(x as u64, y as u64, 1, h as u64, BORDER_COLOR.0, BORDER_COLOR.1, BORDER_COLOR.2);
    canvas.fill_rect((x + w - 1).max(0) as u64, y as u64, 1, h as u64, BORDER_COLOR.0, BORDER_COLOR.1, BORDER_COLOR.2);
}

/// Alpha-blends the real cursor bitmap (see the module docs) at `(mx, my)`
/// onto `canvas` -- the exact per-pixel loop this module's own demo loop
/// already used, now shared with `doom_driver.rs` so a windowed program's
/// own cursor looks and moves identically to the desktop's.
pub fn draw_cursor(canvas: &mut Canvas, mx: i32, my: i32) {
    let cursor_x = mx - cursor::HOTSPOT_X;
    let cursor_y = my - cursor::HOTSPOT_Y;
    for cy in 0..cursor::HEIGHT {
        for cx in 0..cursor::WIDTH {
            if let Some((r, g, b, a)) = cursor::pixel(cx, cy) {
                if a == 0 {
                    continue;
                }
                let px = cursor_x + cx as i32;
                let py = cursor_y + cy as i32;
                if px >= 0 && py >= 0 {
                    canvas.blend_pixel(px as u64, py as u64, r, g, b, a);
                }
            }
        }
    }
}

/// Runs the window manager until Esc is pressed, then restores the screen
/// underneath it. Blocks the calling task for as long as it runs -- there's
/// no separate "GUI task" yet, this is just what the `gui` shell command
/// does synchronously, same as any other command.
pub fn run() {
    let (screen_w, screen_h, background) = {
        let mut console = console::CONSOLE.lock();
        let Some(canvas) = console.canvas_mut() else {
            crate::println!("gui: no framebuffer attached, nothing to draw on");
            return;
        };
        (canvas.width(), canvas.height(), canvas.snapshot())
    };

    let mut windows = alloc::vec::Vec::new();
    windows.push(Window { x: 80, y: 80, w: 260, h: 160, title: "konjac-term", color: (30, 90, 130) });
    windows.push(Window { x: 260, y: 160, w: 220, h: 130, title: "about", color: (110, 60, 130) });

    let mut dragging: Option<(usize, i32, i32)> = None;
    let mut prev_left_down = false;
    let mut last_drawn: Option<(i32, i32, u8)> = None;

    loop {
        if let Some(byte) = keyboard::read_char() {
            if byte == 0x1B {
                break; // Esc.
            }
        }

        let (mx, my) = mouse::position();
        let left_down = mouse::left_button_down();

        if left_down && !prev_left_down {
            // Fresh press: a real close-button click (topmost window
            // whose close button contains the cursor) removes that window
            // outright and never starts a drag. Otherwise, the topmost
            // window (last in z-order) whose title bar contains the
            // cursor starts a drag and jumps to the front.
            if let Some(idx) = windows.iter().rposition(|w| point_in_close_button(w.x, w.y, w.w as i32, mx, my)) {
                windows.remove(idx);
            } else if let Some(idx) = windows.iter().rposition(|w| w.contains_title_bar(mx, my)) {
                let w = windows.remove(idx);
                let offset = (mx - w.x, my - w.y);
                windows.push(w);
                dragging = Some((windows.len() - 1, offset.0, offset.1));
            }
        } else if !left_down {
            dragging = None;
        }

        if let Some((idx, ox, oy)) = dragging {
            if let Some(w) = windows.get_mut(idx) {
                w.x = (mx - ox).clamp(0, screen_w as i32 - w.w as i32);
                w.y = (my - oy).clamp(0, screen_h as i32 - TITLE_BAR_H);
            }
        }
        prev_left_down = left_down;

        let frame_state = (mx, my, mouse::buttons());
        if last_drawn != Some(frame_state) || dragging.is_some() {
            let mut console = console::CONSOLE.lock();
            if let Some(canvas) = console.canvas_mut() {
                canvas.blit(&background);
                for w in &windows {
                    canvas.fill_rect(
                        w.x as u64,
                        (w.y + TITLE_BAR_H) as u64,
                        w.w as u64,
                        (w.h as i32 - TITLE_BAR_H).max(0) as u64,
                        w.color.0,
                        w.color.1,
                        w.color.2,
                    );
                    let hover = point_in_close_button(w.x, w.y, w.w as i32, mx, my);
                    draw_chrome(canvas, w.x, w.y, w.w as i32, w.h as i32, w.title, hover);
                }
                draw_cursor(canvas, mx, my);
            }
            last_drawn = Some(frame_state);
        }

        // Idle until the next interrupt (mouse movement, a keystroke, or
        // just the next timer tick) instead of busy-spinning -- same
        // pattern shell.rs's own main loop uses.
        unsafe {
            core::arch::asm!("hlt");
        }
    }

    let mut console = console::CONSOLE.lock();
    if let Some(canvas) = console.canvas_mut() {
        canvas.blit(&background);
    }
    drop(console);
    crate::println!("gui: back to the shell.");
}
