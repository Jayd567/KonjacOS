//! Minimal driver for the 16550 UART on COM1 (I/O port 0x3F8).
//!
//! QEMU maps COM1 to stdio when run with `-serial stdio` (see
//! `scripts/run.sh`), so this is the easiest way to get text out of the
//! kernel before there's a working framebuffer console.

use core::fmt::{self, Write};

use crate::port::{inb, outb};
use crate::sync::SpinLock;

const COM1: u16 = 0x3F8;

pub struct SerialPort {
    initialized: bool,
}

pub static SERIAL1: SpinLock<SerialPort> = SpinLock::new(SerialPort { initialized: false });

impl SerialPort {
    /// Must be called once, early in `kstart`, before anything prints.
    pub fn init(&mut self) {
        if self.initialized {
            return;
        }
        unsafe {
            outb(COM1 + 1, 0x00); // Disable all interrupts.
            outb(COM1 + 3, 0x80); // Enable DLAB to set the baud rate divisor.
            outb(COM1, 0x01); //      Divisor low byte  -> 115200 baud.
            outb(COM1 + 1, 0x00); //  Divisor high byte.
            outb(COM1 + 3, 0x03); // 8 bits, no parity, one stop bit; DLAB off.
            outb(COM1 + 2, 0xC7); // Enable + clear 14-byte-deep FIFOs.
            outb(COM1 + 4, 0x0B); // IRQs disabled, RTS/DSR set (modem control).
        }
        self.initialized = true;
    }

    #[inline]
    fn transmit_empty(&self) -> bool {
        unsafe { inb(COM1 + 5) & 0x20 != 0 }
    }

    pub fn write_byte(&mut self, byte: u8) {
        if byte == b'\n' {
            // Serial terminals expect CRLF.
            self.write_byte(b'\r');
        }
        while !self.transmit_empty() {
            core::hint::spin_loop();
        }
        unsafe {
            outb(COM1, byte);
        }
    }
}

impl Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
        Ok(())
    }
}

/// Prints to the COM1 serial port.
#[macro_export]
macro_rules! sprint {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::serial::SERIAL1.lock(), $($arg)*);
    }};
}

/// Like [`sprint!`] but appends a newline.
#[macro_export]
macro_rules! sprintln {
    () => { $crate::sprint!("\n") };
    ($($arg:tt)*) => {{
        $crate::sprint!($($arg)*);
        $crate::sprint!("\n");
    }};
}
