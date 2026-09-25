//! Minimal driver for the legacy 8259 Programmable Interrupt Controller
//! pair. We only need this to (a) move hardware IRQs off of vectors 0-31,
//! which are reserved for CPU exceptions, and (b) mask every IRQ except
//! the keyboard's, since nothing else has a handler yet.

use crate::port::{inb, outb};

const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

const ICW1_INIT: u8 = 0x10;
const ICW1_ICW4: u8 = 0x01;
const ICW4_8086: u8 = 0x01;

/// Where remapped IRQs land in the IDT: IRQ0-7 -> vectors 32-39,
/// IRQ8-15 -> vectors 40-47 (the conventional choice -- right after the 32
/// CPU exception vectors).
pub const PIC1_OFFSET: u8 = 32;
pub const PIC2_OFFSET: u8 = 40;

/// # Safety
/// Must only run once, before interrupts are enabled, and must be followed
/// by masking (`set_masks`) before `sti` -- an unmasked IRQ with no IDT
/// entry yet will fault.
pub unsafe fn remap() {
    unsafe {
        // ICW1: start initialization, expect an ICW4.
        outb(PIC1_COMMAND, ICW1_INIT | ICW1_ICW4);
        outb(PIC2_COMMAND, ICW1_INIT | ICW1_ICW4);
        // ICW2: vector offsets.
        outb(PIC1_DATA, PIC1_OFFSET);
        outb(PIC2_DATA, PIC2_OFFSET);
        // ICW3: tell each PIC how they're cascaded (master has a slave on
        // IRQ2 -> bit 2; slave's cascade identity is 2).
        outb(PIC1_DATA, 0b0000_0100);
        outb(PIC2_DATA, 2);
        // ICW4: 8086 mode.
        outb(PIC1_DATA, ICW4_8086);
        outb(PIC2_DATA, ICW4_8086);
    }
}

/// Masks every IRQ except IRQ0 (timer), IRQ1 (keyboard), and IRQ2 (the
/// cascade line the slave PIC relies on -- masking it would also silence
/// anything behind it, though we don't use any slave IRQs either).
///
/// # Safety
/// Must be called after [`remap`].
pub unsafe fn set_masks() {
    unsafe {
        // Bit N = 1 means IRQ N is masked. 0b1111_1000: everything masked
        // except bits 0-2 (timer, keyboard, cascade).
        outb(PIC1_DATA, 0b1111_1000);
        outb(PIC2_DATA, 0xFF);
    }
}

/// Clears the mask bit for a single IRQ (0-15), leaving every other IRQ's
/// mask exactly as `set_masks` (or a previous `unmask`) left it. For a
/// driver installed after boot's initial `set_masks` call -- `mouse.rs`, so
/// far the only one -- that needs its own IRQ enabled without touching
/// anything else.
///
/// # Safety
/// Must be called after [`remap`].
pub unsafe fn unmask(irq: u8) {
    unsafe {
        let port = if irq < 8 { PIC1_DATA } else { PIC2_DATA };
        let bit = irq % 8;
        let current = inb(port);
        outb(port, current & !(1 << bit));
    }
}

/// Sends an End-Of-Interrupt to the PIC(s). Must be called at the end of
/// every IRQ handler, or the PIC will never deliver another interrupt at
/// that priority level or lower.
///
/// # Safety
/// `irq` must be the IRQ number (0-15) that was actually just handled.
pub unsafe fn send_eoi(irq: u8) {
    unsafe {
        if irq >= 8 {
            outb(PIC2_COMMAND, 0x20);
        }
        outb(PIC1_COMMAND, 0x20);
    }
}
