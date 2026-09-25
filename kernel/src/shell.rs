//! A tiny line-based shell: reads characters the keyboard driver has
//! queued, echoes them to the on-screen console, and dispatches whatever
//! was typed through the command table in `commands.rs` on Enter.

use crate::commands;
use crate::console;
use crate::timer;
use crate::{print, println};

const LINE_MAX: usize = 120;
const PROMPT: &str = "konjac> ";
/// How often the cursor flips visibility, in timer ticks. `timer::HZ / 2`
/// ticks is half a second -> a 1-second blink cycle.
const BLINK_INTERVAL_TICKS: u64 = timer::HZ as u64 / 2;

struct LineBuffer {
    buf: [u8; LINE_MAX],
    len: usize,
}

impl LineBuffer {
    const fn new() -> Self {
        LineBuffer { buf: [0; LINE_MAX], len: 0 }
    }

    fn push(&mut self, byte: u8) -> bool {
        if self.len >= LINE_MAX {
            return false;
        }
        self.buf[self.len] = byte;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> bool {
        if self.len == 0 {
            return false;
        }
        self.len -= 1;
        true
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

/// Splits a typed line into a command word and the rest of the line, looks
/// the word up in the command table, and runs it. All the actual command
/// behaviour lives in `commands.rs` -- this is just dispatch.
fn run_command(line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let (command, rest) = match line.split_once(' ') {
        Some((c, r)) => (c, r.trim_start()),
        None => (line, ""),
    };

    match commands::find(command) {
        Some(cmd) => {
            if cmd.requires_apex && !crate::apex::authenticate(cmd.name) {
                println!("{command}: apex authentication failed, aborting.");
                return;
            }
            (cmd.handler)(rest);
        }
        None => println!("unknown command: {command} (try `help`)"),
    }
}

/// Never returns: brings up the prompt and processes keystrokes forever,
/// idling on `hlt` between them so the CPU isn't spinning at 100% waiting
/// for someone to type.
pub fn run() -> ! {
    println!();
    println!("KonjacOS shell. Type `help` for a list of commands.");
    print!("{PROMPT}");

    let mut line = LineBuffer::new();
    let mut last_blink = timer::ticks();

    loop {
        // Blink the cursor if enough time has passed. This runs on every
        // loop iteration, not just when idle, so the cursor keeps blinking
        // even during a burst of fast typing.
        let now = timer::ticks();
        if now.wrapping_sub(last_blink) >= BLINK_INTERVAL_TICKS {
            console::CONSOLE.lock().toggle_cursor();
            last_blink = now;
        }

        match crate::keyboard::read_char() {
            Some(0x08) => {
                // Backspace: only erase if there's something on this line
                // to erase (don't let it eat the prompt).
                if line.pop() {
                    print!("\u{8}");
                }
            }
            Some(b'\n') => {
                println!();
                run_command(line.as_str());
                line.clear();
                print!("{PROMPT}");
            }
            Some(byte) if byte.is_ascii_graphic() || byte == b' ' => {
                if line.push(byte) {
                    console::CONSOLE.lock().write_char(byte as char);
                }
            }
            Some(_) => {} // Unmapped control byte; ignore.
            None => unsafe {
                // Idle until the next interrupt -- either a keystroke or
                // the next timer tick, which is what keeps the blink
                // reasonably responsive without a busy-wait.
                core::arch::asm!("hlt");
            },
        }
    }
}
