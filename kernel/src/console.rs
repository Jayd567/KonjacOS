//! A scrolling text console. This is what the shell prints to --
//! `sprintln!` (serial.rs) is separate and keeps going to COM1 for boot
//! diagnostics.
//!
//! It starts out drawing straight onto the Limine framebuffer with the
//! embedded 8x8 font. Once the desktop takes over the screen it switches
//! to *grid mode* ([`Console::enter_grid`]): text goes into a plain
//! character grid instead, which the desktop's Terminal window renders
//! with an anti-aliased font whenever [`Console::revision`] changes.

extern crate alloc;

use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::font::{GLYPH_HEIGHT, GLYPH_WIDTH};
use crate::framebuffer::Canvas;
use crate::sync::SpinLock;

const FG: (u8, u8, u8) = (220, 220, 220);
const BG: (u8, u8, u8) = (18, 18, 24);

pub struct Console {
    canvas: Option<Canvas>,
    cols: u64,
    rows: u64,
    col: u64,
    row: u64,
    /// Whether the cursor is currently drawn (inverted) at (col, row).
    /// `toggle_cursor` flips this; every cursor-moving operation must call
    /// `hide_cursor` first so a stray inverted block never gets left
    /// behind at a position that's since had other text drawn over it.
    cursor_on: bool,
    /// Grid mode's characters, `cols * rows`, row-major; `None` while the
    /// console still draws on the framebuffer itself.
    grid: Option<Vec<u8>>,
    /// Bumped on every grid change, so the Terminal window knows to redraw.
    revision: u64,
}

impl Console {
    const fn new() -> Self {
        Console {
            canvas: None,
            cols: 0,
            rows: 0,
            col: 0,
            row: 0,
            cursor_on: false,
            grid: None,
            revision: 0,
        }
    }

    /// Switches to grid mode with a `cols x rows` grid and hands back the
    /// framebuffer canvas -- the desktop owns the screen from here on.
    pub fn enter_grid(&mut self, cols: u64, rows: u64) -> Option<Canvas> {
        self.grid = Some(alloc::vec![b' '; (cols * rows) as usize]);
        self.cols = cols;
        self.rows = rows;
        self.col = 0;
        self.row = 0;
        self.cursor_on = false;
        self.revision += 1;
        self.canvas.take()
    }

    /// Resizes the grid (the Terminal window was maximized or restored),
    /// keeping the bottom-most lines that still fit, cursor included.
    pub fn resize_grid(&mut self, cols: u64, rows: u64) {
        let Some(old) = self.grid.take() else { return };
        let mut grid = alloc::vec![b' '; (cols * rows) as usize];
        let keep = rows.min(self.row + 1);
        let first = self.row + 1 - keep;
        for r in 0..keep {
            for c in 0..cols.min(self.cols) {
                grid[(r * cols + c) as usize] = old[((first + r) * self.cols + c) as usize];
            }
        }
        self.row -= first;
        self.col = self.col.min(cols - 1);
        self.cols = cols;
        self.rows = rows;
        self.grid = Some(grid);
        self.revision += 1;
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// `(cols, rows, characters, cursor col, cursor row, cursor visible)`
    /// in grid mode.
    pub fn grid(&self) -> Option<(u64, u64, &[u8], u64, u64, bool)> {
        self.grid.as_deref().map(|g| (self.cols, self.rows, g, self.col, self.row, self.cursor_on))
    }

    /// Must be called once a framebuffer is available, before anything
    /// tries to print through [`CONSOLE`]. Deliberately does *not* clear
    /// the screen -- this lets `main.rs` draw the boot logo first and
    /// have the console's text start below/over it, rather than wiping the
    /// splash before it's ever visible. Call [`Console::clear`] instead if
    /// you want a blank screen.
    pub fn attach(&mut self, canvas: Canvas) {
        self.cols = canvas.width() / GLYPH_WIDTH as u64;
        self.rows = canvas.height() / GLYPH_HEIGHT as u64;
        self.canvas = Some(canvas);
        self.col = 0;
        self.row = 0;
    }

    #[allow(dead_code)] // Part of the public API surface; not used yet.
    pub fn is_attached(&self) -> bool {
        self.canvas.is_some()
    }

    /// Clears the screen and resets the cursor to the top-left corner.
    pub fn clear(&mut self) {
        if let Some(grid) = &mut self.grid {
            grid.fill(b' ');
            self.revision += 1;
        }
        if let Some(canvas) = &mut self.canvas {
            canvas.clear(BG.0, BG.1, BG.2);
        }
        self.col = 0;
        self.row = 0;
        self.cursor_on = false;
    }

    /// Inverts the cell at the current cursor position, drawing it if it
    /// was hidden or hiding it if it was shown. Pixel inversion is
    /// self-cancelling (see `Canvas::invert_rect`), so this is the only
    /// primitive needed in both directions.
    fn invert_cursor_cell(&mut self) {
        if self.grid.is_some() {
            // The Terminal window draws the cursor itself from `cursor_on`.
            self.revision += 1;
            return;
        }
        if let Some(canvas) = &mut self.canvas {
            let x = self.col * GLYPH_WIDTH as u64;
            let y = self.row * GLYPH_HEIGHT as u64;
            canvas.invert_rect(x, y, GLYPH_WIDTH as u64, GLYPH_HEIGHT as u64);
        }
    }

    /// Called by the shell's blink timer, roughly twice a second.
    pub fn toggle_cursor(&mut self) {
        self.invert_cursor_cell();
        self.cursor_on = !self.cursor_on;
    }

    /// Must be called before any operation that moves the cursor or draws
    /// over its cell, so a blinked-on cursor doesn't get left behind as a
    /// stray inverted block once it's no longer where the cursor is.
    fn hide_cursor(&mut self) {
        if self.cursor_on {
            self.invert_cursor_cell();
            self.cursor_on = false;
        }
    }

    fn newline(&mut self) {
        self.hide_cursor();
        self.col = 0;
        if self.row + 1 >= self.rows {
            if let Some(grid) = &mut self.grid {
                let cols = self.cols as usize;
                grid.copy_within(cols.., 0);
                let len = grid.len();
                grid[len - cols..].fill(b' ');
                self.revision += 1;
            } else if let Some(canvas) = &mut self.canvas {
                canvas.scroll_up(GLYPH_HEIGHT as u64, BG);
            }
        } else {
            self.row += 1;
        }
    }

    fn backspace(&mut self) {
        self.hide_cursor();
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.cols - 1;
        } else {
            return;
        }
        self.put(' ');
    }

    /// Writes one character at the cursor without moving it.
    fn put(&mut self, c: char) {
        if let Some(grid) = &mut self.grid {
            let b = if c.is_ascii() && !c.is_ascii_control() { c as u8 } else { b'?' };
            grid[(self.row * self.cols + self.col) as usize] = b;
            self.revision += 1;
        } else if let Some(canvas) = &mut self.canvas {
            let x = self.col * GLYPH_WIDTH as u64;
            let y = self.row * GLYPH_HEIGHT as u64;
            canvas.draw_char(x, y, c, FG, BG);
        }
    }

    pub fn write_char(&mut self, c: char) {
        if self.canvas.is_none() && self.grid.is_none() {
            return;
        }
        match c {
            '\n' => self.newline(),
            '\r' => {}
            '\u{8}' => self.backspace(),
            '\t' => {
                for _ in 0..(4 - self.col % 4) {
                    self.write_char(' ');
                }
            }
            _ => {
                self.hide_cursor();
                self.put(c);
                self.col += 1;
                if self.col >= self.cols {
                    self.newline();
                }
            }
        }
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            self.write_char(c);
        }
        Ok(())
    }
}

pub static CONSOLE: SpinLock<Console> = SpinLock::new(Console::new());

/// Prints to the on-screen console. A no-op (not a panic) if the console
/// hasn't been attached to a framebuffer yet -- callers don't need to
/// special-case early boot.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::console::CONSOLE.lock(), $($arg)*);
    }};
}

/// Like [`print!`] but appends a newline.
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        $crate::print!($($arg)*);
        $crate::print!("\n");
    }};
}
