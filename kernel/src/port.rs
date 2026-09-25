//! Thin wrappers around the x86 `in`/`out` instructions.

use core::arch::asm;

/// # Safety
/// Reading from an arbitrary I/O port can have side effects and is only
/// safe if `port` refers to a device register the caller knows how to
/// drive.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    value
}

/// # Safety
/// See [`inb`]: writing to an arbitrary I/O port can have arbitrary
/// side effects.
#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Reads `buf.len() / 2` 16-bit words from `port` into `buf`, low byte
/// first -- the shape the ATA PIO data port hands sectors back in.
///
/// # Safety
/// See [`inb`]: `port` must be a register that's actually ready to be read
/// as a stream of words (e.g. an ATA data port mid-transfer).
#[inline]
pub unsafe fn insw(port: u16, buf: &mut [u8]) {
    debug_assert!(buf.len() % 2 == 0, "insw: buffer length must be a multiple of 2");
    unsafe {
        for chunk in buf.chunks_exact_mut(2) {
            let word: u16;
            asm!("in ax, dx", out("ax") word, in("dx") port, options(nomem, nostack, preserves_flags));
            chunk[0] = (word & 0xFF) as u8;
            chunk[1] = (word >> 8) as u8;
        }
    }
}

/// Writes `buf.len() / 2` 16-bit words to `port`, low byte first -- the
/// write-side counterpart of [`insw`], for pushing a sector's worth of
/// data into the ATA PIO data port.
///
/// # Safety
/// See [`outb`]: `port` must be a register that's actually ready to
/// accept a stream of words (e.g. an ATA data port mid-transfer).
#[inline]
pub unsafe fn outsw(port: u16, buf: &[u8]) {
    debug_assert!(buf.len() % 2 == 0, "outsw: buffer length must be a multiple of 2");
    unsafe {
        for chunk in buf.chunks_exact(2) {
            let word = u16::from(chunk[0]) | (u16::from(chunk[1]) << 8);
            asm!("out dx, ax", in("dx") port, in("ax") word, options(nomem, nostack, preserves_flags));
        }
    }
}
