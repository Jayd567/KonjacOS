//! IDT (Interrupt Descriptor Table) and CPU exception handlers.
//!
//! Until this runs, any CPU exception -- a page fault, a divide by zero, an
//! invalid opcode, anything -- has nowhere to go. The CPU tries to deliver
//! it through an empty IDT, that itself faults, and the cascade ends in a
//! triple fault: the whole machine silently resets. `init()` below installs
//! a real IDT with a handler for all 32 CPU exception vectors, so a bug
//! turns into a readable "here's what went wrong and where" message on the
//! serial console instead of a mysterious reboot.
//!
//! There's no recovery here -- every handler prints and halts. Resuming
//! execution after a fault is a much harder problem (you'd need to either
//! fix up whatever caused it or unwind out of it), and isn't needed yet:
//! the point of this module is turning invisible failures into visible
//! ones while the rest of the kernel is being built.

use core::arch::global_asm;
use core::mem::size_of;

use crate::gdt::{DOUBLE_FAULT_IST_INDEX, KERNEL_CODE_SELECTOR};
use crate::sprintln;

const IDT_ENTRIES: usize = 256;
/// CPU exceptions occupy vectors 0-31; everything from here up is free for
/// hardware IRQs (`pic.rs` remaps them starting at 32) or, eventually,
/// software interrupts.
const NUM_EXCEPTION_VECTORS: usize = 32;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const fn missing() -> Self {
        IdtEntry {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    /// `handler` is one of the `isr_stub_N` labels defined in the
    /// `global_asm!` block below. `ist` is an IST index (1-7) to force a
    /// stack switch on entry, or 0 to stay on whatever stack was active.
    /// `dpl` is the *minimum* privilege level allowed to reach this gate via
    /// a software `int n` -- irrelevant for hardware IRQs and CPU
    /// exceptions (the CPU ignores it for those), but it's the one thing
    /// standing between ring 3 and an immediate #GP if it ever tries `int
    /// 0x80`: 0 everywhere except the syscall gate, which needs 3.
    fn new(handler: u64, ist: u8, dpl: u8) -> Self {
        IdtEntry {
            offset_low: handler as u16,
            selector: KERNEL_CODE_SELECTOR,
            ist,
            // type 0xE = 64-bit interrupt gate (clears IF on entry, unlike
            // a trap gate -- we're not re-enabling interrupts inside these
            // handlers, so it doesn't matter much, but it's the
            // conventional choice). Present + DPL occupy the top bits.
            type_attr: 0x8E | (dpl << 5),
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }
}

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry::missing(); IDT_ENTRIES];

/// # Safety
/// Must only be called once, early in `kstart`, after `gdt::init()` (the
/// IDT entries point at `KERNEL_CODE_SELECTOR`, which must already be
/// loaded) and before anything that might fault.
pub unsafe fn init() {
    macro_rules! stub_addr {
        ($n:ident) => {{
            unsafe extern "C" {
                fn $n();
            }
            $n as *const () as u64
        }};
    }

    unsafe {
        let stubs: [u64; NUM_EXCEPTION_VECTORS] = [
            stub_addr!(isr_stub_0),
            stub_addr!(isr_stub_1),
            stub_addr!(isr_stub_2),
            stub_addr!(isr_stub_3),
            stub_addr!(isr_stub_4),
            stub_addr!(isr_stub_5),
            stub_addr!(isr_stub_6),
            stub_addr!(isr_stub_7),
            stub_addr!(isr_stub_8),
            stub_addr!(isr_stub_9),
            stub_addr!(isr_stub_10),
            stub_addr!(isr_stub_11),
            stub_addr!(isr_stub_12),
            stub_addr!(isr_stub_13),
            stub_addr!(isr_stub_14),
            stub_addr!(isr_stub_15),
            stub_addr!(isr_stub_16),
            stub_addr!(isr_stub_17),
            stub_addr!(isr_stub_18),
            stub_addr!(isr_stub_19),
            stub_addr!(isr_stub_20),
            stub_addr!(isr_stub_21),
            stub_addr!(isr_stub_22),
            stub_addr!(isr_stub_23),
            stub_addr!(isr_stub_24),
            stub_addr!(isr_stub_25),
            stub_addr!(isr_stub_26),
            stub_addr!(isr_stub_27),
            stub_addr!(isr_stub_28),
            stub_addr!(isr_stub_29),
            stub_addr!(isr_stub_30),
            stub_addr!(isr_stub_31),
        ];

        for (vector, &addr) in stubs.iter().enumerate() {
            let ist = if vector == 8 { DOUBLE_FAULT_IST_INDEX as u8 } else { 0 };
            IDT[vector] = IdtEntry::new(addr, ist, 0);
        }

        let pointer = DescriptorTablePointer {
            limit: (size_of::<[IdtEntry; IDT_ENTRIES]>() - 1) as u16,
            base: core::ptr::addr_of!(IDT) as u64,
        };
        core::arch::asm!("lidt [{}]", in(reg) &pointer);
    }
}

/// Installs a handler at an arbitrary vector (32-255 -- the exception
/// vectors below that are owned by `init()`). Used for hardware IRQ stubs
/// like `keyboard.rs`'s `isr_stub_33`.
///
/// Since `init()` already pointed `lidt` at this same static array with a
/// limit covering all 256 entries, writing a new entry here takes effect
/// immediately -- no need to reload the IDT register.
///
/// # Safety
/// Must be called after `init()`. `handler` must be the address of a valid
/// interrupt entry point matching the calling convention `isr_common`/the
/// per-IRQ stubs use (raw, no Rust ABI).
pub unsafe fn set_handler(vector: usize, handler: u64) {
    assert!(vector >= NUM_EXCEPTION_VECTORS, "vector {vector} is a reserved CPU exception");
    unsafe {
        IDT[vector] = IdtEntry::new(handler, 0, 0);
    }
}

/// Like [`set_handler`], but installs a DPL=3 gate: reachable via `int n`
/// from ring 3 without an immediate #GP. So far only `syscall.rs`'s
/// vector needs this -- everything else has no business being triggerable
/// from user code.
///
/// # Safety
/// Same as [`set_handler`].
pub unsafe fn set_user_handler(vector: usize, handler: u64) {
    assert!(vector >= NUM_EXCEPTION_VECTORS, "vector {vector} is a reserved CPU exception");
    unsafe {
        IDT[vector] = IdtEntry::new(handler, 0, 3);
    }
}

/// Name for each CPU exception vector, per the Intel SDM.
fn diagnostic_bytes<const N: usize>(addr: u64) -> Option<[u8; N]> {
    let end = addr.checked_add(N as u64)?;
    if end > 0x0000_8000_0000_0000 { return None; }
    let cr3 = crate::paging::current_cr3();
    let hhdm = crate::pmm::hhdm_offset();
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        let at = addr + i as u64;
        // Safety: current address space is live with IRQs off; translate only
        // walks resident tables and the returned physical frame is HHDM mapped.
        let phys = unsafe { crate::paging::translate(cr3, at) }?;
        *byte = unsafe { ((hhdm + phys + (at & 4095)) as *const u8).read() };
    }
    Some(out)
}

fn exception_name(vector: u64) -> &'static str {
    match vector {
        0 => "Divide Error",
        1 => "Debug",
        2 => "Non-Maskable Interrupt",
        3 => "Breakpoint",
        4 => "Overflow",
        5 => "BOUND Range Exceeded",
        6 => "Invalid Opcode",
        7 => "Device Not Available",
        8 => "Double Fault",
        9 => "Coprocessor Segment Overrun",
        10 => "Invalid TSS",
        11 => "Segment Not Present",
        12 => "Stack-Segment Fault",
        13 => "General Protection Fault",
        14 => "Page Fault",
        16 => "x87 Floating-Point Exception",
        17 => "Alignment Check",
        18 => "Machine Check",
        19 => "SIMD Floating-Point Exception",
        20 => "Virtualization Exception",
        21 => "Control Protection Exception",
        _ => "Reserved/Unknown Exception",
    }
}

/// Called by every `isr_stub_N` (via `isr_common` in the asm below) with
/// the vector number, the hardware error code (0 if the vector doesn't
/// have one), the faulting RIP, and the faulting frame's saved `CS` --
/// which is what actually decides what happens next (see below). Never
/// returns.
///
/// Every *other* vector than #PF used to be unconditionally fatal to the
/// whole machine, ring 3 or not -- an honest, deliberate simplification
/// while this kernel's only ring-3 programs were small, hand-verified
/// demos that were never expected to fault at all outside of #PF's own
/// already-handled mmap/`SIGSEGV` cases. That stopped being a safe
/// simplification the moment a real, unmodified `java` binary (see
/// README items 33-35) got far enough to run genuinely new, unverified
/// machine code -- HotSpot's own thread-startup path -- and hit a real
/// #GP this kernel had never seen before. Halting the *entire* system
/// over one ring-3 task's bug, when every other task (including this
/// shell) has nothing to do with it, is exactly the kind of blast-radius
/// mismatch real Linux doesn't have either: a real unhandled #GP in a
/// user process is `SIGSEGV`/process death, not a kernel panic.
///
/// So: `CS`'s low 2 bits are the CPL the fault actually happened at
/// (Intel SDM Vol. 3A -- a segment selector's RPL, and `CS`'s in
/// particular is always the code segment's own current privilege level).
/// `CPL == 3` means this was ring-3 user code -- killed via
/// [`task::task_exit`], the exact same "mark Terminated, `schedule()`
/// away, never resume this stack" mechanism `SYS_EXIT`/a kernel thread
/// returning normally already use, safe to call here for the same reason
/// it's safe from `timer.rs`'s own IRQ0 handler: both run with `IF=0`,
/// both are willing to abandon whatever's currently on this stack (the
/// dying task's own registers, which nothing will ever read back), and
/// `switch_to`'s callee-saved push/pop only ever needs the *incoming*
/// task's saved state to be valid, never the outgoing one's. `CPL == 0`
/// (a fault in the kernel itself) keeps the original, unconditional
/// halt -- that's still a real, unrecoverable kernel bug, not something
/// to paper over by killing some arbitrary "current" task.
#[unsafe(no_mangle)]
extern "C" fn exception_handler(vector: u64, error_code: u64, rip: u64, cs: u64, regs: *const u64, user_rsp: u64) -> ! {
    let cr2 = if vector == 14 {
        let value: u64;
        unsafe {
            core::arch::asm!("mov {}, cr2", out(reg) value);
        }
        Some(value)
    } else {
        None
    };

    let from_ring3 = cs & 3 == 3;

    sprintln!();
    sprintln!(
        "*** CPU EXCEPTION: #{vector} {} ***",
        exception_name(vector)
    );
    sprintln!("  error code: {error_code:#x}");
    sprintln!("  faulting RIP: {rip:#x}");
    if let Some(addr) = cr2 {
        sprintln!("  faulting address (CR2): {addr:#x}");
    }

    if from_ring3 {
        // Ascending-address order out of `isr_common`'s own push sequence
        // (rax first pushed, r15 last -- so r15 ends up lowest, at
        // `regs[0]`) -- a real, if minimal, register dump: exactly the
        // kind of ground truth real Linux's own oops/core-dump gives you
        // for free and this kernel never bothered to before now, since
        // every fault before this one was fatal to the whole machine
        // anyway and there was nothing left to debug *with*.
        let r = |i: usize| unsafe { regs.add(i).read() };
        sprintln!("  rax={:#018x} rbx={:#018x} rcx={:#018x} rdx={:#018x}", r(14), r(13), r(12), r(11));
        sprintln!("  rsi={:#018x} rdi={:#018x} rbp={:#018x}", r(10), r(9), r(8));
        sprintln!("  r8 ={:#018x} r9 ={:#018x} r10={:#018x} r11={:#018x}", r(7), r(6), r(5), r(4));
        sprintln!("  r12={:#018x} r13={:#018x} r14={:#018x} r15={:#018x}", r(3), r(2), r(1), r(0));
        // Diagnostics must never fault while formatting under SERIAL1. An
        // invalid RIP/RSP (or physical exhaustion) is precisely why we may
        // be here. Read only already-present pages through the resident HHDM.
        if let Some(code) = diagnostic_bytes::<16>(rip) {
            sprintln!("  code at RIP: {code:02x?}");
        } else {
            sprintln!("  code at RIP: unavailable");
        }
        // If this fault happened landing on a bad *call* target (the most
        // likely shape for "jumped to a data address instead of real
        // code" -- see this item's own README account), the caller's real
        // return address is sitting right on top of this task's own stack
        // -- `call` always pushes it before jumping. Dumping a handful of
        // words there costs nothing and, unlike everything above, points
        // straight at *which real function* made the bad indirect call,
        // not just where it landed.
        sprintln!("  user RSP={user_rsp:#x}, top of stack:");
        for i in 0..6u64 {
            if let Some(bytes) = user_rsp.checked_add(i * 8).and_then(diagnostic_bytes::<8>) {
                let word = u64::from_le_bytes(bytes);
                sprintln!("    [rsp+{:#04x}] = {word:#018x}", i * 8);
            } else {
                sprintln!("    [rsp+{:#04x}] = unavailable", i * 8);
            }
        }
        // Real Linux kills the *whole* thread group on an uncaught fatal
        // signal, not just the one thread that took it -- without this, a
        // clone3'd sibling thread that outlives its dead thread-group
        // leader (see README item 40) keeps this task's cr3 looking
        // "shared" forever, which permanently blocks schedule()'s reaping
        // sweep from ever freeing it (see task::kill_group's own doc
        // comment, and item 42's real memory-leak/OOM-panic finding).
        let cr3 = crate::task::current_cr3();
        let id = crate::task::current_id();
        crate::task::kill_group(cr3, id);
        sprintln!("  ring 3 (CS={cs:#x}) -- killing task #{id} and the rest of its thread group, system stays up.");
        crate::task::task_exit(); // Never returns -- see this function's own doc comment.
    }

    sprintln!("  ring 0 (CS={cs:#x}) -- a real kernel bug, not recoverable. halting.");
    unsafe {
        core::arch::asm!("cli");
    }
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

// Generates `isr_stub_0` through `isr_stub_31`, *except* `isr_stub_14`
// (#PF): each pushes a dummy error code (0) if the CPU doesn't push a real
// one for that vector, then the vector number itself, then falls into the
// shared `isr_common` trampoline, which only ever prints and halts. #PF is
// the one exception this kernel can actually recover from (a first touch
// of `SYS_MMAP`-reserved memory), so it needs a real resumable handler
// instead -- see `paging.rs`'s hand-written `isr_stub_14`, which fully
// save/restores GPR state and can `iretq` back into the faulting
// instruction, unlike every other vector here.
//
// Vectors 8, 10-13, and 17 are the other ones where the CPU pushes a real
// 32-bit error code automatically (Intel SDM Vol. 3A, section 6.15) --
// everything else gets a synthetic 0 so every stub leaves the stack in the
// same shape: [vector, error_code, RIP, CS, RFLAGS, (RSP, SS if a stack
// switch happened)].
global_asm!(
    r#"
.altmacro

.macro isr_no_err num
.global isr_stub_\num
isr_stub_\num:
    push 0
    push \num
    jmp isr_common
.endm

.macro isr_err num
.global isr_stub_\num
isr_stub_\num:
    push \num
    jmp isr_common
.endm

.set i, 0
.rept 32
    .if i == 14
        # isr_stub_14 is hand-written in paging.rs -- see there.
    .elseif i == 8 || i == 10 || i == 11 || i == 12 || i == 13 || i == 17
        isr_err %i
    .else
        isr_no_err %i
    .endif
    .set i, i + 1
.endr

isr_common:
    # Push every GPR *before* touching the original hardware frame, purely
    # for `exception_handler`'s own ring-3 diagnostic dump (see its doc
    # comment) -- still no need to ever *restore* them, since a ring-3 kill
    # abandons this stack for good either way, and a ring-0 fault still
    # just halts. `rsp` after these 15 pushes points right at the block
    # exception_handler reads back as its 5th argument.
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
    # The original frame (vector, error_code, RIP, CS, RFLAGS, RSP, SS) now
    # sits 15*8=120 bytes above rsp -- RSP/SS (offsets 160/168) are only
    # real for a fault that actually crossed privilege levels (any ring-3
    # one), since the CPU only pushes them itself on a real ring change;
    # exception_handler only ever reads r9 back when it's already known to
    # be a ring-3 fault (CS's own RPL bits), so reading it unconditionally
    # here (whatever garbage sits there for a ring-0 fault, never used) is
    # harmless and keeps this stub branch-free.
    mov rdi, [rsp + 120]
    mov rsi, [rsp + 128]
    mov rdx, [rsp + 136]
    mov rcx, [rsp + 144]
    mov r9, [rsp + 160]
    mov r8, rsp
    call exception_handler
    # exception_handler is `-> !` and never returns, but just in case:
    cli
1:  hlt
    jmp 1b
"#
);
