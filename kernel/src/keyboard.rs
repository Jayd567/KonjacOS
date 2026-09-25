//! PS/2 keyboard driver: translates IBM PC "scancode set 1" (what the PS/2
//! controller emits, and what QEMU's emulated keyboard emits too) into
//! ASCII, and hands characters to whoever's polling [`read_char`] -- the
//! shell's input loop.
//!
//! The actual IRQ1 entry point is a hand-written assembly stub
//! (`isr_stub_33` in the `global_asm!` below) rather than a Rust function
//! directly, for the same reason `idt.rs`'s exception stubs are assembly:
//! the CPU calls into this with no defined Rust calling convention, so
//! something has to save every register that might be live in whatever
//! this interrupted, call into Rust, and restore them all -- including the
//! SSE registers via `fxsave`/`fxrstor`, since ordinary Rust code (as
//! discussed in `boot.rs`) can use them for plain data moves, not just
//! float math, and an interrupt can land between two instructions that
//! assumed an SSE register would survive.

use core::arch::global_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::idt;
use crate::pic;
use crate::port::inb;

const DATA_PORT: u16 = 0x60;
const IRQ_KEYBOARD: u8 = 1;

// --- Scancode (set 1) -> ASCII -------------------------------------------

/// Index = scancode's "make code" (key press). 0 means "no ASCII mapping"
/// (function keys, arrows, modifiers, ...) -- the shell ignores those.
static SCANCODE_ASCII: [u8; 128] = {
    let mut table = [0u8; 128];
    // This has to be written as index assignments (rather than a nicer
    // array literal) because `const` evaluation doesn't have a convenient
    // sparse-array syntax; see the printable ASCII rows below.
    macro_rules! set {
        ($($code:expr => $ch:expr),* $(,)?) => {
            $(table[$code] = $ch;)*
        };
    }
    set! {
        0x01 => 0x1B, // Esc
        0x02 => b'1', 0x03 => b'2', 0x04 => b'3', 0x05 => b'4', 0x06 => b'5',
        0x07 => b'6', 0x08 => b'7', 0x09 => b'8', 0x0A => b'9', 0x0B => b'0',
        0x0C => b'-', 0x0D => b'=',
        0x0E => 0x08, // Backspace
        0x0F => b'\t',
        0x10 => b'q', 0x11 => b'w', 0x12 => b'e', 0x13 => b'r', 0x14 => b't',
        0x15 => b'y', 0x16 => b'u', 0x17 => b'i', 0x18 => b'o', 0x19 => b'p',
        0x1A => b'[', 0x1B => b']',
        0x1C => b'\n', // Enter
        0x1E => b'a', 0x1F => b's', 0x20 => b'd', 0x21 => b'f', 0x22 => b'g',
        0x23 => b'h', 0x24 => b'j', 0x25 => b'k', 0x26 => b'l',
        0x27 => b';', 0x28 => b'\'', 0x29 => b'`',
        0x2B => b'\\',
        0x2C => b'z', 0x2D => b'x', 0x2E => b'c', 0x2F => b'v', 0x30 => b'b',
        0x31 => b'n', 0x32 => b'm',
        0x33 => b',', 0x34 => b'.', 0x35 => b'/',
        0x39 => b' ',
    }
    table
};

/// Same layout as `SCANCODE_ASCII`, but what each key produces with Shift
/// held.
static SCANCODE_ASCII_SHIFT: [u8; 128] = {
    let mut table = [0u8; 128];
    macro_rules! set {
        ($($code:expr => $ch:expr),* $(,)?) => {
            $(table[$code] = $ch;)*
        };
    }
    set! {
        0x02 => b'!', 0x03 => b'@', 0x04 => b'#', 0x05 => b'$', 0x06 => b'%',
        0x07 => b'^', 0x08 => b'&', 0x09 => b'*', 0x0A => b'(', 0x0B => b')',
        0x0C => b'_', 0x0D => b'+',
        0x10 => b'Q', 0x11 => b'W', 0x12 => b'E', 0x13 => b'R', 0x14 => b'T',
        0x15 => b'Y', 0x16 => b'U', 0x17 => b'I', 0x18 => b'O', 0x19 => b'P',
        0x1A => b'{', 0x1B => b'}',
        0x1E => b'A', 0x1F => b'S', 0x20 => b'D', 0x21 => b'F', 0x22 => b'G',
        0x23 => b'H', 0x24 => b'J', 0x25 => b'K', 0x26 => b'L',
        0x27 => b':', 0x28 => b'"', 0x29 => b'~',
        0x2B => b'|',
        0x2C => b'Z', 0x2D => b'X', 0x2E => b'C', 0x2F => b'V', 0x30 => b'B',
        0x31 => b'N', 0x32 => b'M',
        0x33 => b'<', 0x34 => b'>', 0x35 => b'?',
    }
    table
};

/// Scancode (set 1) -> DOOM keycode (`doomkeys.h`'s `KEY_*` constants,
/// mirrored here as plain numbers since this file has no reason to
/// depend on C headers), used only by [`DG_GetKey`]/the doom event ring.
/// This is the same mapping doomgeneric's own DOS/i_input.c driver uses
/// for AT scancodes (`at_to_doom[]`) -- letters/digits/punctuation are
/// their own unshifted ASCII (DOOM does its own shift handling via a
/// separate "typed char" path that this driver doesn't need to feed),
/// and everything else is one of the `KEY_*` special codes.
static SCANCODE_DOOMKEY: [u8; 128] = {
    let mut table = [0u8; 128];
    macro_rules! set {
        ($($code:expr => $ch:expr),* $(,)?) => {
            $(table[$code] = $ch;)*
        };
    }
    set! {
        0x01 => 27,     // KEY_ESCAPE
        0x02 => b'1', 0x03 => b'2', 0x04 => b'3', 0x05 => b'4', 0x06 => b'5',
        0x07 => b'6', 0x08 => b'7', 0x09 => b'8', 0x0A => b'9', 0x0B => b'0',
        0x0C => b'-', 0x0D => b'=',
        0x0E => 0x7f,   // KEY_BACKSPACE
        0x0F => 9,      // KEY_TAB
        0x10 => b'q', 0x11 => b'w', 0x12 => b'e', 0x13 => b'r', 0x14 => b't',
        0x15 => b'y', 0x16 => b'u', 0x17 => b'i', 0x18 => b'o', 0x19 => b'p',
        0x1A => b'[', 0x1B => b']',
        0x1C => 13,     // KEY_ENTER
        0x1D => 0xa3,   // KEY_FIRE (Ctrl)
        0x1E => b'a', 0x1F => b's', 0x20 => b'd', 0x21 => b'f', 0x22 => b'g',
        0x23 => b'h', 0x24 => b'j', 0x25 => b'k', 0x26 => b'l',
        0x27 => b';', 0x28 => b'\'', 0x29 => b'`',
        0x2A => 0x80u8.wrapping_add(0x36), // KEY_RSHIFT (left shift shares the same code)
        0x2B => b'\\',
        0x2C => b'z', 0x2D => b'x', 0x2E => b'c', 0x2F => b'v', 0x30 => b'b',
        0x31 => b'n', 0x32 => b'm',
        0x33 => b',', 0x34 => b'.', 0x35 => b'/',
        0x36 => 0x80u8.wrapping_add(0x36), // KEY_RSHIFT (right shift)
        0x38 => 0x80u8.wrapping_add(0x38), // KEY_LALT/KEY_RALT
        0x39 => 0xa2,   // KEY_USE (Space)
        0x3B => 0x80u8.wrapping_add(0x3b), // KEY_F1
        0x3C => 0x80u8.wrapping_add(0x3c), // KEY_F2
        0x3D => 0x80u8.wrapping_add(0x3d), // KEY_F3
        0x3E => 0x80u8.wrapping_add(0x3e), // KEY_F4
        0x3F => 0x80u8.wrapping_add(0x3f), // KEY_F5
        0x40 => 0x80u8.wrapping_add(0x40), // KEY_F6
        0x41 => 0x80u8.wrapping_add(0x41), // KEY_F7
        0x42 => 0x80u8.wrapping_add(0x42), // KEY_F8
        0x43 => 0x80u8.wrapping_add(0x43), // KEY_F9
        0x44 => 0x80u8.wrapping_add(0x44), // KEY_F10
        0x48 => 0xad,   // KEY_UPARROW (via the non-extended numpad-8 code;
                         // QEMU/most PS/2 emulation also sends the E0-
                         // prefixed "real" arrow keys, which this simple
                         // single-byte table doesn't special-case yet)
        0x4B => 0xac,   // KEY_LEFTARROW
        0x4D => 0xae,   // KEY_RIGHTARROW
        0x50 => 0xaf,   // KEY_DOWNARROW
    }
    table
};

/// Ring buffer of `(pressed, doomkey)` pairs feeding `DG_GetKey` (see
/// `doomgeneric_konjac.c`), entirely separate from the ASCII `RING` above
/// that feeds the shell's `read_char`. Both are populated from the same
/// IRQ1 handler; the doom ring simply goes unread (and never fills, since
/// nothing pushes to it) whenever DOOM isn't running.
const DOOM_RING_SIZE: usize = 64;
static mut DOOM_RING: [u16; DOOM_RING_SIZE] = [0; DOOM_RING_SIZE];
static DOOM_RING_HEAD: AtomicUsize = AtomicUsize::new(0);
static DOOM_RING_TAIL: AtomicUsize = AtomicUsize::new(0);

fn doom_ring_push(pressed: bool, doomkey: u8) {
    let head = DOOM_RING_HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % DOOM_RING_SIZE;
    if next == DOOM_RING_TAIL.load(Ordering::Acquire) {
        return; // Full; drop the event rather than overwrite unread data.
    }
    let encoded = ((pressed as u16) << 8) | doomkey as u16;
    unsafe {
        DOOM_RING[head] = encoded;
    }
    DOOM_RING_HEAD.store(next, Ordering::Release);
}

/// Drops every currently-queued DOOM key event. Called right before DOOM
/// launches (see `commands.rs`'s `cmd_doom`) so that whatever's still
/// sitting in the queue from typing the `doom` command itself and
/// pressing Enter to run it -- keystrokes the DOOM ring buffer above
/// collects unconditionally, same as the ASCII one, with no way to tell
/// "shell input" from "game input" apart at the IRQ level -- doesn't get
/// misread as DOOM's own first inputs (e.g. that trailing Enter
/// immediately opening the in-game menu).
pub fn clear_doom_events() {
    let head = DOOM_RING_HEAD.load(Ordering::Acquire);
    DOOM_RING_TAIL.store(head, Ordering::Release);
}

/// Pops the next `(pressed, doomkey)` event for `DG_GetKey`. Returns
/// `None` when the queue is empty.
pub fn read_doom_event() -> Option<(bool, u8)> {
    let tail = DOOM_RING_TAIL.load(Ordering::Relaxed);
    if tail == DOOM_RING_HEAD.load(Ordering::Acquire) {
        return None;
    }
    let encoded = unsafe { DOOM_RING[tail] };
    DOOM_RING_TAIL.store((tail + 1) % DOOM_RING_SIZE, Ordering::Release);
    Some((encoded >> 8 != 0, (encoded & 0xff) as u8))
}

const LEFT_SHIFT_MAKE: u8 = 0x2A;
const LEFT_SHIFT_BREAK: u8 = 0xAA;
const RIGHT_SHIFT_MAKE: u8 = 0x36;
const RIGHT_SHIFT_BREAK: u8 = 0xB6;

static SHIFT_HELD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// --- Single-producer/single-consumer ring buffer -------------------------
//
// Producer: the IRQ1 handler (interrupt context). Consumer: the shell's
// input loop (normal context, polling). This works lock-free because
// there's exactly one of each and IF is clear for the whole time the
// producer runs, so the two never truly run concurrently on this
// single-core kernel -- but we still use atomics with acquire/release so
// this stays correct if that ever changes (e.g. this moves to run on an
// AP later).

const RING_SIZE: usize = 128;
static mut RING: [u8; RING_SIZE] = [0; RING_SIZE];
static RING_HEAD: AtomicUsize = AtomicUsize::new(0); // next slot to write
static RING_TAIL: AtomicUsize = AtomicUsize::new(0); // next slot to read

fn ring_push(byte: u8) {
    let head = RING_HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % RING_SIZE;
    if next == RING_TAIL.load(Ordering::Acquire) {
        return; // Full; drop the keystroke rather than overwrite unread data.
    }
    unsafe {
        RING[head] = byte;
    }
    RING_HEAD.store(next, Ordering::Release);
}

/// Pops the next available character, if any. Non-blocking -- the shell's
/// main loop calls this repeatedly (with `hlt` in between to idle).
pub fn read_char() -> Option<u8> {
    let tail = RING_TAIL.load(Ordering::Relaxed);
    if tail == RING_HEAD.load(Ordering::Acquire) {
        return None;
    }
    let byte = unsafe { RING[tail] };
    RING_TAIL.store((tail + 1) % RING_SIZE, Ordering::Release);
    Some(byte)
}

/// # Safety
/// Must only be called once, after `gdt::init()`/`idt::init()`, and before
/// `sti`.
pub unsafe fn init() {
    unsafe extern "C" {
        fn isr_stub_33();
    }
    unsafe {
        pic::remap();
        pic::set_masks();
        idt::set_handler(
            pic::PIC1_OFFSET as usize + IRQ_KEYBOARD as usize,
            isr_stub_33 as *const () as u64,
        );
    }
}

/// Called by `isr_stub_33` for every keyboard interrupt. Reads the
/// scancode, updates shift state or pushes a translated character, and
/// sends the PIC an EOI so it'll deliver the next one.
#[unsafe(no_mangle)]
extern "C" fn irq1_handler() {
    let scancode = unsafe { inb(DATA_PORT) };

    match scancode {
        LEFT_SHIFT_MAKE | RIGHT_SHIFT_MAKE => {
            SHIFT_HELD.store(true, Ordering::Relaxed);
        }
        LEFT_SHIFT_BREAK | RIGHT_SHIFT_BREAK => {
            SHIFT_HELD.store(false, Ordering::Relaxed);
        }
        code if code < 0x80 => {
            let shift = SHIFT_HELD.load(Ordering::Relaxed);
            let table = if shift { &SCANCODE_ASCII_SHIFT } else { &SCANCODE_ASCII };
            let ch = table[code as usize];
            if ch != 0 {
                ring_push(ch);
            }
        }
        _ => {} // Key release of a non-shift key: nothing to do.
    }

    // Feed the separate DOOM key-event ring alongside the ASCII one above,
    // regardless of whether this scancode had an ASCII mapping -- DOOM
    // needs press *and* release events for movement keys, which the
    // ASCII ring above never reports at all (it's make-code-only).
    let (pressed, code) = if scancode < 0x80 { (true, scancode) } else { (false, scancode - 0x80) };
    if (code as usize) < 128 {
        let doomkey = SCANCODE_DOOMKEY[code as usize];
        if doomkey != 0 {
            doom_ring_push(pressed, doomkey);
        }
    }

    unsafe {
        pic::send_eoi(IRQ_KEYBOARD);
    }
}

global_asm!(
    r#"
.section .bss
.align 16
KEYBOARD_FXSAVE_AREA:
    .skip 512
.section .text

.global isr_stub_33
isr_stub_33:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    fxsave [rip + KEYBOARD_FXSAVE_AREA]
    call irq1_handler
    fxrstor [rip + KEYBOARD_FXSAVE_AREA]
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    iretq
"#
);
