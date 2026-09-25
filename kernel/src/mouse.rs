//! PS/2 mouse driver -- the "auxiliary device" on the same 8042 controller
//! the keyboard already uses. `init()` enables it, unmasks IRQ12, and
//! installs a handler that decodes the classic 3-byte packet format into a
//! clamped on-screen cursor position + button state, which `wm.rs` polls
//! every frame.

use core::sync::atomic::{AtomicI32, AtomicU8, Ordering};

use crate::idt;
use crate::pic;
use crate::port::{inb, outb};

const CONTROLLER_STATUS: u16 = 0x64;
const CONTROLLER_COMMAND: u16 = 0x64;
const CONTROLLER_DATA: u16 = 0x60;
const IRQ_MOUSE: u8 = 12;

const CMD_ENABLE_AUX: u8 = 0xA8;
const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
/// Tells the 8042 controller that the *next* byte written to the data port
/// should be routed to the mouse, not treated as a keyboard command.
const CMD_WRITE_TO_MOUSE: u8 = 0xD4;
const MOUSE_SET_DEFAULTS: u8 = 0xF6;
const MOUSE_ENABLE_REPORTING: u8 = 0xF4;

pub const LEFT_BUTTON: u8 = 1 << 0;
#[allow(dead_code)] // Read via buttons(); not everything using this driver cares about every button yet.
pub const RIGHT_BUTTON: u8 = 1 << 1;
#[allow(dead_code)]
pub const MIDDLE_BUTTON: u8 = 1 << 2;

static POS_X: AtomicI32 = AtomicI32::new(0);
static POS_Y: AtomicI32 = AtomicI32::new(0);
static BUTTONS: AtomicU8 = AtomicU8::new(0);
static SCREEN_W: AtomicI32 = AtomicI32::new(1);
static SCREEN_H: AtomicI32 = AtomicI32::new(1);

// Packet assembly state. Single producer (IRQ12, which runs with
// interrupts disabled for its own duration) and single consumer (nothing
// reads these directly -- only the fully-decoded POS_X/POS_Y/BUTTONS
// above), so plain `static mut` is safe here the same way keyboard.rs's
// ring buffer reasons about its own producer side.
static mut PACKET: [u8; 3] = [0; 3];
static mut PACKET_INDEX: u8 = 0;

unsafe fn wait_write_ready() {
    unsafe {
        while inb(CONTROLLER_STATUS) & 0x02 != 0 {}
    }
}

unsafe fn wait_read_ready() {
    unsafe {
        while inb(CONTROLLER_STATUS) & 0x01 == 0 {}
    }
}

unsafe fn controller_write(port: u16, value: u8) {
    unsafe {
        wait_write_ready();
        outb(port, value);
    }
}

unsafe fn mouse_write(byte: u8) {
    unsafe {
        controller_write(CONTROLLER_COMMAND, CMD_WRITE_TO_MOUSE);
        controller_write(CONTROLLER_DATA, byte);
    }
}

unsafe fn mouse_read_ack() -> u8 {
    unsafe {
        wait_read_ready();
        inb(CONTROLLER_DATA)
    }
}

/// # Safety
/// Must run after `keyboard::init()` (needs `pic::remap`/`set_masks`
/// already done -- this only adds IRQ12 to what they set up) and before
/// `sti`. `screen_w`/`screen_h` clamp the cursor position and should be the
/// real framebuffer dimensions.
pub unsafe fn init(screen_w: u64, screen_h: u64) {
    SCREEN_W.store(screen_w.max(1) as i32, Ordering::Relaxed);
    SCREEN_H.store(screen_h.max(1) as i32, Ordering::Relaxed);
    POS_X.store(screen_w as i32 / 2, Ordering::Relaxed);
    POS_Y.store(screen_h as i32 / 2, Ordering::Relaxed);

    unsafe {
        controller_write(CONTROLLER_COMMAND, CMD_ENABLE_AUX);

        controller_write(CONTROLLER_COMMAND, CMD_READ_CONFIG);
        wait_read_ready();
        let mut config = inb(CONTROLLER_DATA);
        config |= 0b0000_0010; // Enable IRQ12 on a mouse event.
        config &= !0b0010_0000; // Make sure the mouse's clock isn't disabled.
        controller_write(CONTROLLER_COMMAND, CMD_WRITE_CONFIG);
        controller_write(CONTROLLER_DATA, config);

        mouse_write(MOUSE_SET_DEFAULTS);
        mouse_read_ack();
        mouse_write(MOUSE_ENABLE_REPORTING);
        mouse_read_ack();

        unsafe extern "C" {
            fn isr_stub_44();
        }
        idt::set_handler(pic::PIC2_OFFSET as usize + (IRQ_MOUSE - 8) as usize, isr_stub_44 as *const () as u64);
        pic::unmask(IRQ_MOUSE);
    }
}

/// Current cursor position, already clamped to the screen.
pub fn position() -> (i32, i32) {
    (POS_X.load(Ordering::Relaxed), POS_Y.load(Ordering::Relaxed))
}

/// Bitmask of currently-held buttons (`LEFT_BUTTON` etc).
pub fn buttons() -> u8 {
    BUTTONS.load(Ordering::Relaxed)
}

pub fn left_button_down() -> bool {
    buttons() & LEFT_BUTTON != 0
}

/// Called by `isr_stub_44` for every mouse interrupt. Assembles the classic
/// 3-byte packet (button flags + sign bits, dx, dy), and on the third byte
/// updates the public position/button state. Y is inverted (PS/2 reports
/// "moved up" as a positive delta; screen Y grows downward), and out-of-
/// sync bytes (missing the packet's required marker bit) are dropped
/// rather than let a single dropped interrupt permanently misalign every
/// packet after it.
#[unsafe(no_mangle)]
extern "C" fn irq12_handler() {
    let byte = unsafe { inb(CONTROLLER_DATA) };
    unsafe {
        if PACKET_INDEX == 0 && byte & 0x08 == 0 {
            pic::send_eoi(IRQ_MOUSE);
            return;
        }
        PACKET[PACKET_INDEX as usize] = byte;
        PACKET_INDEX += 1;
        if PACKET_INDEX == 3 {
            PACKET_INDEX = 0;
            let flags = PACKET[0];
            let mut dx = PACKET[1] as i32;
            let mut dy = PACKET[2] as i32;
            if flags & 0x10 != 0 {
                dx -= 256; // Sign-extend dx from its 9th bit in flags.
            }
            if flags & 0x20 != 0 {
                dy -= 256; // Sign-extend dy from its 9th bit in flags.
            }

            let max_x = SCREEN_W.load(Ordering::Relaxed) - 1;
            let max_y = SCREEN_H.load(Ordering::Relaxed) - 1;
            let nx = (POS_X.load(Ordering::Relaxed) + dx).clamp(0, max_x.max(0));
            let ny = (POS_Y.load(Ordering::Relaxed) - dy).clamp(0, max_y.max(0));
            POS_X.store(nx, Ordering::Relaxed);
            POS_Y.store(ny, Ordering::Relaxed);
            BUTTONS.store(flags & 0x07, Ordering::Relaxed);
        }
    }
    unsafe {
        pic::send_eoi(IRQ_MOUSE);
    }
}

// Same shape as keyboard.rs's isr_stub_33 / timer.rs's isr_stub_32: full
// GPR save + fxsave/fxrstor around the Rust handler. A fixed buffer (not
// task.rs's per-task indirection) is correct here, same reasoning as
// keyboard.rs: this IRQ never calls schedule()/switch_to, so there's no
// risk of a different task's own interrupt reusing this buffer before this
// one's fxrstor runs.
core::arch::global_asm!(
    r#"
.section .bss
.align 16
MOUSE_FXSAVE_AREA:
    .skip 512
.section .text

.global isr_stub_44
isr_stub_44:
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
    fxsave [rip + MOUSE_FXSAVE_AREA]
    call irq12_handler
    fxrstor [rip + MOUSE_FXSAVE_AREA]
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
