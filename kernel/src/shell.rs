//! The prompt: reads characters the keyboard driver has queued, echoes
//! them to the on-screen console, and runs each line through ks
//! (KonjacShell, the `ks` crate) on Enter. The kernel side of ks lives in
//! `ks_host.rs`.

use crate::console;
use crate::ks_host::KernelHost;
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

/// Runs a typed line through ks and shows the result, or the error with
/// the line underlined where it went wrong.
fn run_line(engine: &mut ks::Engine, line: &str) {
    if line.trim().is_empty() {
        return;
    }
    // A Ctrl+C pressed at the prompt isn't meant for this line.
    crate::keyboard::take_interrupt();
    let mut host = KernelHost;
    match engine.run(line, &mut host) {
        Ok(ks::Value::Nothing) => {}
        Ok(v) => {
            let width = ks::Host::width(&mut host);
            println!("{}", ks::display::render(&v, width));
        }
        Err(e) => println!("{}", engine.render_error(&e)),
    }
}

/// Never returns: brings up the prompt and processes keystrokes forever,
/// sleeping a tick between polls so the CPU isn't spinning waiting for
/// someone to type.
pub fn run() -> ! {
    println!();
    println!("KonjacOS shell (ks). Type `help` for a list of commands.");
    print!("{PROMPT}");

    let mut engine = crate::ks_host::engine();
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

        if crate::keyboard::take_interrupt() {
            // Ctrl+C at the prompt: drop the line.
            println!("^C");
            line.clear();
            print!("{PROMPT}");
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
                run_line(&mut engine, line.as_str());
                line.clear();
                print!("{PROMPT}");
            }
            Some(byte) if byte.is_ascii_graphic() || byte == b' ' => {
                if line.push(byte) {
                    console::CONSOLE.lock().write_char(byte as char);
                }
            }
            Some(_) => {} // Unmapped control byte; ignore.
            // Nothing typed: sleep until the next timer tick (10ms --
            // imperceptible as typing latency), giving the CPU to other
            // tasks rather than holding on to this timeslice.
            None => crate::task::sleep_ticks(1),
        }
    }
}
