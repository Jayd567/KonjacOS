//! Programmable Interval Timer (PIT, Intel 8253/8254) driver.
//!
//! Programs channel 0 to fire IRQ0 at a fixed rate and counts ticks -- the
//! only thing that gives the kernel any sense of time passing. Used for
//! the `uptime` shell command and the console's blinking cursor.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::idt;
use crate::pic;
use crate::port::outb;

const PIT_CHANNEL0: u16 = 0x40;
const PIT_COMMAND: u16 = 0x43;
const PIT_BASE_FREQUENCY: u32 = 1_193_182;

/// How many times per second IRQ0 fires. 100 Hz is plenty for a blink
/// timer and an uptime counter without generating pointless interrupt
/// traffic.
pub const HZ: u32 = 100;

const IRQ_TIMER: u8 = 0;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Ticks since `init()`. Divide by [`HZ`] for seconds.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

pub fn uptime_seconds() -> u64 {
    ticks() / HZ as u64
}

/// # Safety
/// Must only be called once, after `idt::init()` and `pic::remap()`
/// (`keyboard::init()` already does the latter), and before `sti`.
pub unsafe fn init() {
    unsafe extern "C" {
        fn isr_stub_32();
    }

    let divisor = PIT_BASE_FREQUENCY / HZ;
    unsafe {
        // Channel 0, lobyte/hibyte access, mode 3 (square wave), binary.
        outb(PIT_COMMAND, 0x36);
        outb(PIT_CHANNEL0, (divisor & 0xFF) as u8);
        outb(PIT_CHANNEL0, (divisor >> 8) as u8);

        idt::set_handler(
            pic::PIC1_OFFSET as usize + IRQ_TIMER as usize,
            isr_stub_32 as *const () as u64,
        );
    }
}

#[unsafe(no_mangle)]
extern "C" fn irq0_handler() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    unsafe {
        pic::send_eoi(IRQ_TIMER);
    }
    // Every tick is a preemption point: round-robins to the next Ready
    // task, if there is one. See task.rs's module docs for how this
    // composes safely with the fxsave/fxrstor below -- short version: this
    // call might not return here until a *different* task's own next timer
    // tick, and when it finally does, it's because that's genuinely this
    // task's turn again.
    crate::task::schedule();
}

// Same shape as keyboard.rs's isr_stub_33 (full GPR save plus fxsave/
// fxrstor for the SSE/x87 state, since this can interrupt anything), except
// the fxsave/fxrstor target isn't a fixed buffer -- it's read indirectly
// through task.rs's CURRENT_FXSAVE_PTR, and deliberately re-read *after*
// `call irq0_handler` rather than reused, since that call may have switched
// which task is running (see task.rs). A single fixed buffer would let one
// task's timer interrupt overwrite another's not-yet-restored FPU state.
global_asm!(
    r#"
.section .text

.global isr_stub_32
isr_stub_32:
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
    mov rax, [rip + CURRENT_FXSAVE_PTR]
    fxsave [rax]
    call irq0_handler
    mov rax, [rip + CURRENT_FXSAVE_PTR]
    fxrstor [rax]
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
