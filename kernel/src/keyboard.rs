//! PS/2 keyboard driver: translates IBM PC "scancode set 1" (what the PS/2
//! controller emits, and what QEMU's emulated keyboard emits too) into
//! ASCII, and hands characters to whoever's polling [`read_char`] -- the
//! shell's input loop.
//!
//! Three destinations, decided per keystroke in the interrupt handler:
//!
//! - **The desktop** ([`read_desktop_key`]): its shortcuts (Alt+Tab,
//!   Alt+F4, Super and Super+key, Ctrl+Alt+T), and every key while it has
//!   asked for them with [`set_capture`] (a menu or the Alt+Tab switcher
//!   is open, or an app's text field -- Settings' search -- has focus).
//!   These never reach the shell or DOOM.
//! - **DOOM**, while its window has focus ([`set_doom_focus`]): press and
//!   release events, through its own ring.
//! - **The shell** otherwise: ASCII characters.
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
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
/// Prefix byte for the "extended" keys: arrows, right Ctrl/Alt, Super...
const EXTENDED_PREFIX: u8 = 0xE0;
const CTRL_CODE: u8 = 0x1D;
const ALT_CODE: u8 = 0x38;
const CAPS_CODE: u8 = 0x3A;
/// Left and right Super ("Windows") keys, both E0-prefixed.
const SUPER_L_CODE: u8 = 0x5B;
const SUPER_R_CODE: u8 = 0x5C;

static SHIFT_HELD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static CTRL_HELD: AtomicBool = AtomicBool::new(false);
static ALT_HELD: AtomicBool = AtomicBool::new(false);
static SUPER_HELD: AtomicBool = AtomicBool::new(false);
/// The last byte was `EXTENDED_PREFIX`.
static EXTENDED: AtomicBool = AtomicBool::new(false);
/// Super went down and nothing else has been pressed since: releasing it
/// is a tap (opens Start), not the end of a Super+key shortcut.
static SUPER_ALONE: AtomicBool = AtomicBool::new(false);
/// An Alt+Tab was handled while Alt has been down, so the desktop wants
/// to hear when Alt comes back up (that's when the switcher commits).
static ALT_TABBING: AtomicBool = AtomicBool::new(false);
/// The desktop wants every key (a menu or the switcher is open).
static CAPTURE: AtomicBool = AtomicBool::new(false);
static CAPS_ON: AtomicBool = AtomicBool::new(false);

// --- Keys for the desktop -----------------------------------------------------

/// Desktop key codes: printable keys are their lowercase ASCII; these are
/// the rest.
pub const KEY_ESC: u8 = 1;
pub const KEY_ENTER: u8 = 2;
pub const KEY_TAB: u8 = 3;
pub const KEY_UP: u8 = 4;
pub const KEY_DOWN: u8 = 5;
pub const KEY_LEFT: u8 = 6;
pub const KEY_RIGHT: u8 = 7;
pub const KEY_F4: u8 = 8;
/// Super pressed and released on its own.
pub const KEY_SUPER: u8 = 9;
/// Alt released after an Alt+Tab.
pub const KEY_ALT_UP: u8 = 10;
pub const KEY_BACKSPACE: u8 = 11;
pub const KEY_DELETE: u8 = 12;
pub const KEY_HOME: u8 = 13;
pub const KEY_END: u8 = 14;
pub const KEY_PAGE_UP: u8 = 15;
pub const KEY_PAGE_DOWN: u8 = 16;
pub const KEY_F2: u8 = 17;
pub const KEY_F5: u8 = 18;

pub const MOD_SHIFT: u8 = 1;
pub const MOD_CTRL: u8 = 2;
pub const MOD_ALT: u8 = 4;
pub const MOD_SUPER: u8 = 8;
/// Caps Lock is on.
pub const MOD_CAPS: u8 = 16;

/// The character a printable desktop key `code` (its unshifted ASCII)
/// types with `mods` held: Shift (or Caps Lock, for letters) gives the
/// upper row.
pub fn typed_char(code: u8, mods: u8) -> u8 {
    let shift = mods & MOD_SHIFT != 0;
    if code.is_ascii_lowercase() {
        return if shift != (mods & MOD_CAPS != 0) { code.to_ascii_uppercase() } else { code };
    }
    if shift {
        if let Some(i) = SCANCODE_ASCII.iter().position(|&c| c == code) {
            if SCANCODE_ASCII_SHIFT[i] != 0 {
                return SCANCODE_ASCII_SHIFT[i];
            }
        }
    }
    code
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DesktopKey {
    pub code: u8,
    /// `MOD_*` bits held when it was pressed.
    pub mods: u8,
}

const DESK_RING_SIZE: usize = 32;
static mut DESK_RING: [u16; DESK_RING_SIZE] = [0; DESK_RING_SIZE];
static DESK_RING_HEAD: AtomicUsize = AtomicUsize::new(0);
static DESK_RING_TAIL: AtomicUsize = AtomicUsize::new(0);

fn desk_ring_push(code: u8, mods: u8) {
    let head = DESK_RING_HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % DESK_RING_SIZE;
    if next == DESK_RING_TAIL.load(Ordering::Acquire) {
        return;
    }
    unsafe {
        DESK_RING[head] = ((mods as u16) << 8) | code as u16;
    }
    DESK_RING_HEAD.store(next, Ordering::Release);
}

/// The next shortcut or captured key for the desktop, if any.
pub fn read_desktop_key() -> Option<DesktopKey> {
    let tail = DESK_RING_TAIL.load(Ordering::Relaxed);
    if tail == DESK_RING_HEAD.load(Ordering::Acquire) {
        return None;
    }
    let v = unsafe { DESK_RING[tail] };
    DESK_RING_TAIL.store((tail + 1) % DESK_RING_SIZE, Ordering::Release);
    Some(DesktopKey { code: (v & 0xff) as u8, mods: (v >> 8) as u8 })
}

/// While on, every key goes to the desktop instead of the shell or DOOM.
pub fn set_capture(on: bool) {
    CAPTURE.store(on, Ordering::Relaxed);
}

fn mods_now() -> u8 {
    let mut m = 0;
    if SHIFT_HELD.load(Ordering::Relaxed) {
        m |= MOD_SHIFT;
    }
    if CTRL_HELD.load(Ordering::Relaxed) {
        m |= MOD_CTRL;
    }
    if ALT_HELD.load(Ordering::Relaxed) {
        m |= MOD_ALT;
    }
    if SUPER_HELD.load(Ordering::Relaxed) {
        m |= MOD_SUPER;
    }
    if CAPS_ON.load(Ordering::Relaxed) {
        m |= MOD_CAPS;
    }
    m
}

/// The desktop key code for a make code, or 0 if the desktop has no use
/// for it. Arrows arrive both E0-prefixed and as plain numpad codes.
fn desktop_code(code: u8) -> u8 {
    match code {
        0x01 => KEY_ESC,
        0x1C => KEY_ENTER,
        0x0F => KEY_TAB,
        0x48 => KEY_UP,
        0x50 => KEY_DOWN,
        0x4B => KEY_LEFT,
        0x4D => KEY_RIGHT,
        0x3E => KEY_F4,
        0x0E => KEY_BACKSPACE,
        0x53 => KEY_DELETE,
        0x47 => KEY_HOME,
        0x4F => KEY_END,
        0x49 => KEY_PAGE_UP,
        0x51 => KEY_PAGE_DOWN,
        0x3C => KEY_F2,
        0x3F => KEY_F5,
        c if c < 0x80 && (0x20..0x7f).contains(&SCANCODE_ASCII[c as usize]) => SCANCODE_ASCII[c as usize],
        _ => 0,
    }
}

/// Whether a key pressed with `mods` held is one of the desktop's own
/// shortcuts rather than input for the shell or DOOM.
fn is_shortcut(key: u8, mods: u8) -> bool {
    if key == 0 {
        return false;
    }
    mods & MOD_SUPER != 0
        || (mods & MOD_ALT != 0 && (key == KEY_TAB || key == KEY_F4))
        || (mods & (MOD_CTRL | MOD_ALT) == MOD_CTRL | MOD_ALT && key == b't')
}

/// Handles modifier and desktop keys. Returns `true` if the scancode was
/// used up here and must not reach the shell or DOOM.
fn desktop_filter(scancode: u8, extended: bool) -> bool {
    let pressed = scancode < 0x80;
    let code = scancode & 0x7f;
    match code {
        SUPER_L_CODE | SUPER_R_CODE if extended => {
            if pressed {
                if !SUPER_HELD.swap(true, Ordering::Relaxed) {
                    SUPER_ALONE.store(true, Ordering::Relaxed);
                }
            } else {
                SUPER_HELD.store(false, Ordering::Relaxed);
                if SUPER_ALONE.swap(false, Ordering::Relaxed) {
                    desk_ring_push(KEY_SUPER, 0);
                }
            }
            return true;
        }
        // The fake shifts some keyboards wrap extended keys in.
        0x2A | 0x36 if extended => return true,
        CTRL_CODE => CTRL_HELD.store(pressed, Ordering::Relaxed),
        CAPS_CODE if pressed => {
            CAPS_ON.fetch_xor(true, Ordering::Relaxed);
        }
        ALT_CODE => {
            ALT_HELD.store(pressed, Ordering::Relaxed);
            if !pressed && ALT_TABBING.swap(false, Ordering::Relaxed) {
                desk_ring_push(KEY_ALT_UP, 0);
            }
        }
        _ => {}
    }
    if !pressed {
        return false;
    }
    let modifier = matches!(code, CTRL_CODE | ALT_CODE | 0x2A | 0x36);
    if !modifier {
        SUPER_ALONE.store(false, Ordering::Relaxed);
    }
    let key = desktop_code(code);
    let mods = mods_now();
    if is_shortcut(key, mods) || (CAPTURE.load(Ordering::Relaxed) && key != 0) {
        if key == KEY_TAB && mods & MOD_ALT != 0 {
            ALT_TABBING.store(true, Ordering::Relaxed);
        }
        desk_ring_push(key, mods);
        return true;
    }
    false
}

/// Where keystrokes go: `false` = the ASCII ring (the shell, via the
/// desktop's Terminal window), `true` = the DOOM event ring. The desktop
/// flips this as keyboard focus moves between windows, so typing in the
/// Terminal doesn't steer DOOM and vice versa.
static DOOM_FOCUS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn set_doom_focus(doom: bool) {
    let had = DOOM_FOCUS.load(Ordering::Relaxed);
    if doom && !had {
        clear_doom_events();
    }
    if !doom && had {
        // DOOM won't see the releases of keys still held as focus moves
        // away (Alt, after an Alt+Tab, most of all): release them now so
        // it doesn't keep strafing or running when it comes back.
        for code in [0x1D, 0x2A, 0x36, 0x38, 0x48, 0x4B, 0x4D, 0x50] {
            let doomkey = SCANCODE_DOOMKEY[code];
            if doomkey != 0 {
                doom_ring_push(false, doomkey);
            }
        }
    }
    DOOM_FOCUS.store(doom, Ordering::Relaxed);
}

/// Queues `text` as if it had been typed -- how the desktop's menus run
/// shell commands (`reboot`, `cat <file>`, ...) through the real shell, so
/// they get its normal behaviour, apex password prompt included.
pub fn inject(text: &str) {
    // The ring is single-producer (the IRQ handler); keep it that way by
    // pushing with interrupts off.
    unsafe { core::arch::asm!("cli") };
    for b in text.bytes() {
        ring_push(b);
    }
    unsafe { core::arch::asm!("sti") };
}

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

/// Set by Ctrl+C, cleared by [`take_interrupt`].
static INTERRUPT: AtomicBool = AtomicBool::new(false);

/// Whether Ctrl+C was pressed since the last call.
pub fn take_interrupt() -> bool {
    INTERRUPT.swap(false, Ordering::Relaxed)
}

/// Called by `isr_stub_33` for every keyboard interrupt. Reads the
/// scancode, updates shift state or pushes a translated character, and
/// sends the PIC an EOI so it'll deliver the next one.
#[unsafe(no_mangle)]
extern "C" fn irq1_handler() {
    let scancode = unsafe { inb(DATA_PORT) };

    if scancode == EXTENDED_PREFIX {
        EXTENDED.store(true, Ordering::Relaxed);
        unsafe { pic::send_eoi(IRQ_KEYBOARD) };
        return;
    }
    let extended = EXTENDED.swap(false, Ordering::Relaxed);
    if desktop_filter(scancode, extended) {
        unsafe { pic::send_eoi(IRQ_KEYBOARD) };
        return;
    }

    match scancode {
        LEFT_SHIFT_MAKE | RIGHT_SHIFT_MAKE => {
            SHIFT_HELD.store(true, Ordering::Relaxed);
        }
        LEFT_SHIFT_BREAK | RIGHT_SHIFT_BREAK => {
            SHIFT_HELD.store(false, Ordering::Relaxed);
        }
        code if code < 0x80 && !DOOM_FOCUS.load(Ordering::Relaxed) => {
            let shift = SHIFT_HELD.load(Ordering::Relaxed);
            let table = if shift { &SCANCODE_ASCII_SHIFT } else { &SCANCODE_ASCII };
            let ch = table[code as usize];
            if ch == b'c' && CTRL_HELD.load(Ordering::Relaxed) {
                // Ctrl+C: stops whatever the shell is running, or cancels
                // the line at the prompt (both poll `take_interrupt`).
                INTERRUPT.store(true, Ordering::Relaxed);
            } else if ch != 0 {
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
    if (code as usize) < 128 && DOOM_FOCUS.load(Ordering::Relaxed) {
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
