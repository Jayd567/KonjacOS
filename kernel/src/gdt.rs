//! A GDT (Global Descriptor Table) and TSS (Task State Segment) we own.
//!
//! Limine hands us a working GDT, but it's Limine's, not ours -- it can be
//! reclaimed/overwritten once we're past boot, and it has no TSS, which we
//! need for `idt.rs`'s double-fault handler to run on its own known-good
//! stack (via the IST mechanism) instead of whatever RSP happened to be
//! when things went wrong. So: build our own, right after boot.

use core::arch::asm;
use core::mem::size_of;

/// Selector for our kernel code segment (index 1 in the GDT).
pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
/// Selector for our kernel data segment (index 2).
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
/// Selector for the TSS (index 3, but 16 bytes long -- it eats two slots).
const TSS_SELECTOR: u16 = 0x18;

/// Which IST (Interrupt Stack Table) slot the double-fault handler uses.
/// IST slots are 1-indexed; 0 means "don't switch stacks".
pub const DOUBLE_FAULT_IST_INDEX: u16 = 1;

/// Ring-3 code/data selectors -- what a task's CS/SS actually get set to
/// when `task.rs` drops it into user mode. `| 3` sets the selector's RPL to
/// 3 (requested privilege level), which is what actually makes it a ring-3
/// reference; the descriptor's own DPL (baked into `USER_CODE_DESCRIPTOR`/
/// `USER_DATA_DESCRIPTOR` below) has to independently say 3 too, or the CPU
/// would refuse to honour the RPL and fault instead.
pub const USER_CODE_SELECTOR: u16 = 0x28 | 3;
pub const USER_DATA_SELECTOR: u16 = 0x30 | 3;

/// A second, otherwise-identical set of ring-3 code/data descriptors
/// (0x40/0x48), used *only* by [`linux_syscall`]'s `sysretq` path -- not
/// because a flat, DPL=3, long-mode descriptor needs to differ from
/// [`USER_CODE_SELECTOR`]/[`USER_DATA_SELECTOR`] in any way that actually
/// matters (it doesn't; x86_64 ignores base/limit for code/data segments
/// in long mode, so these two pairs describe the exact same thing), but
/// because the `STAR` MSR's `SYSRET` half is hard-wired by the CPU to a
/// *fixed layout*: `SS = STAR[63:48]+8`, `CS = STAR[63:48]+16`. That's the
/// opposite order from `USER_CODE_SELECTOR`/`USER_DATA_SELECTOR` above
/// (code then data), so reusing them would need `STAR[63:48]` to satisfy
/// two contradictory equations at once. Simplest fix: don't reuse them --
/// lay out one more pair in the order `SYSRET` actually wants (a spare,
/// never-loaded slot at the base, then data, then code) and point `STAR`
/// at that instead. See `Gdt`'s layout below and `linux_syscall::init`'s
/// `STAR` value.
const SYSRET_BASE: u16 = 0x38;
pub const SYSRET_USER_DATA_SELECTOR: u16 = (SYSRET_BASE + 8) | 3;
pub const SYSRET_USER_CODE_SELECTOR: u16 = (SYSRET_BASE + 16) | 3;

const DOUBLE_FAULT_STACK_SIZE: usize = 4096 * 4;

/// Storage for the double-fault stack. A `static mut`-equivalent via
/// `UnsafeCell`-free plain `static` works here because nothing ever reads
/// or writes this array through Rust -- only the CPU pushes to it once RSP
/// is pointed here by the TSS, entirely outside Rust's aliasing rules.
#[repr(align(16))]
struct Stack([u8; DOUBLE_FAULT_STACK_SIZE]);
static mut DOUBLE_FAULT_STACK: Stack = Stack([0; DOUBLE_FAULT_STACK_SIZE]);

#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

impl Tss {
    const fn new() -> Self {
        Tss {
            reserved0: 0,
            rsp: [0; 3],
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            // No I/O permission bitmap -- point it past the end of the TSS,
            // which is the documented way to say "there isn't one".
            iomap_base: size_of::<Tss>() as u16,
        }
    }
}

static mut TSS: Tss = Tss::new();

/// A flat (base=0, limit=0) code/data segment descriptor. In 64-bit mode
/// the CPU ignores base/limit for code and data segments -- the only bit
/// that matters for a code segment is `L` (long mode) in `flags`.
const fn flat_descriptor(access: u8, flags: u8) -> u64 {
    ((flags as u64 & 0x0F) << 52) | (access as u64) << 40
}

const NULL_DESCRIPTOR: u64 = 0;
// Present, ring 0, code, non-conforming, readable; long-mode (L) bit set.
const CODE_DESCRIPTOR: u64 = flat_descriptor(0x9A, 0x2);
// Present, ring 0, data, writable.
const DATA_DESCRIPTOR: u64 = flat_descriptor(0x92, 0x0);
// Same shape as the ring-0 versions above, but DPL=3 (access byte bits 5-6)
// instead of DPL=0 -- 0x9A|0x60=0xFA, 0x92|0x60=0xF2. This is what actually
// lets code run at CPL=3 at all: without a DPL=3 descriptor to load into
// CS, there's nowhere for a task.rs `iretq` into user mode to point.
const USER_CODE_DESCRIPTOR: u64 = flat_descriptor(0xFA, 0x2);
const USER_DATA_DESCRIPTOR: u64 = flat_descriptor(0xF2, 0x0);

#[repr(C, packed)]
struct Gdt {
    null: u64,
    code: u64,
    data: u64,
    tss_low: u64,
    tss_high: u64,
    // The TSS descriptor above is 16 bytes (two slots, indices 3-4, ending
    // at offset 0x28), so these start at 0x28/0x30 -- matching
    // USER_CODE_SELECTOR/USER_DATA_SELECTOR above exactly.
    user_code: u64,
    user_data: u64,
    // SYSRET_BASE (0x38) onwards -- see SYSRET_USER_DATA_SELECTOR/
    // SYSRET_USER_CODE_SELECTOR's doc comment for why `sysretq` needs its
    // own pair instead of reusing user_code/user_data above. sysret_pad is
    // never actually loaded into a segment register (it stands in for the
    // 32-bit-mode user CS a `sysretq` to 64-bit mode never uses) -- it
    // only exists so sysret_data/sysret_code land at the fixed +8/+16
    // offsets STAR's high half assumes.
    sysret_pad: u64,
    sysret_data: u64,
    sysret_code: u64,
}

static mut GDT: Gdt = Gdt {
    null: NULL_DESCRIPTOR,
    code: CODE_DESCRIPTOR,
    data: DATA_DESCRIPTOR,
    tss_low: 0,
    tss_high: 0,
    user_code: USER_CODE_DESCRIPTOR,
    user_data: USER_DATA_DESCRIPTOR,
    sysret_pad: NULL_DESCRIPTOR,
    sysret_data: USER_DATA_DESCRIPTOR,
    sysret_code: USER_CODE_DESCRIPTOR,
};

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Builds the two 8-byte halves of a 16-byte TSS system-segment descriptor.
fn tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let low = (limit as u64 & 0xFFFF)
        | ((base & 0xFFFFFF) << 16)
        | (0x89u64 << 40) // present, ring 0, type=1001 (64-bit TSS, available)
        | (((limit as u64 >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);
    let high = (base >> 32) & 0xFFFF_FFFF;
    (low, high)
}

/// # Safety
/// Must only be called once, early in `kstart`, before interrupts are
/// enabled (they aren't yet -- Limine leaves them off) and before anything
/// relies on our GDT/TSS being live.
pub unsafe fn init() {
    unsafe {
        let df_stack_top = core::ptr::addr_of!(DOUBLE_FAULT_STACK.0)
            as u64
            + DOUBLE_FAULT_STACK_SIZE as u64;
        TSS.ist[(DOUBLE_FAULT_IST_INDEX - 1) as usize] = df_stack_top;

        let tss_base = core::ptr::addr_of!(TSS) as u64;
        let (tss_low, tss_high) = tss_descriptor(tss_base, (size_of::<Tss>() - 1) as u32);
        GDT.tss_low = tss_low;
        GDT.tss_high = tss_high;

        let pointer = DescriptorTablePointer {
            limit: (size_of::<Gdt>() - 1) as u16,
            base: core::ptr::addr_of!(GDT) as u64,
        };

        asm!(
            "lgdt [{ptr}]",
            // Reload CS: you can't `mov cs, ax`, so push the new selector
            // and target address, then far-return into it.
            "lea rax, [rip + 2f]",
            "push {code_sel}",
            "push rax",
            "retfq",
            "2:",
            // Reload the rest of the segment registers.
            "mov ax, {data_sel:x}",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            // Load the task register with our TSS selector.
            "mov ax, {tss_sel:x}",
            "ltr ax",
            ptr = in(reg) &pointer,
            code_sel = const KERNEL_CODE_SELECTOR,
            data_sel = in(reg) KERNEL_DATA_SELECTOR,
            tss_sel = in(reg) TSS_SELECTOR,
            out("rax") _,
        );
    }
}

/// Mirrors whatever `set_kernel_stack` last pointed the TSS's RSP0 at --
/// but readable from raw assembly by a stable symbol name, which `TSS`
/// itself (a private, unmangled `static mut`) isn't. `linux_syscall.rs`'s
/// hand-written `syscall`/`sysretq` entry point needs this: unlike an
/// `int 0x80`/exception gate, the `syscall` instruction does *not*
/// automatically switch to the TSS's RSP0 on a ring3->ring0 transition --
/// there is no hardware stack switch at all, so the entry stub has to load
/// a real kernel stack pointer itself, from *somewhere*, before it's safe
/// to push anything. This is that somewhere: the exact same per-task
/// kernel stack `int 0x80` already gets for free via the TSS, just also
/// exposed as a plain global the syscall entry asm can `mov rsp, [rip +
/// SYSCALL_KERNEL_RSP]` from directly.
#[unsafe(no_mangle)]
pub static mut SYSCALL_KERNEL_RSP: u64 = 0;

/// Writes a Model-Specific Register -- the mechanism [`crate::linux_syscall`]
/// uses to enable `syscall`/`sysretq` at all (`EFER.SCE`) and to tell the
/// CPU where to land (`STAR`/`LSTAR`/`SFMASK`), and [`crate::task`] uses on
/// every context switch to point `FS_BASE` at whichever task is about to
/// run's own TLS block. `wrmsr` takes the MSR index in `ecx` and the
/// 64-bit value split across `edx:eax` (high:low) -- not a single 64-bit
/// register, unlike almost everything else in long mode.
///
/// # Safety
/// Caller must pass a valid MSR index for this CPU and a value that
/// register actually accepts; an unsupported index or a reserved-bit
/// violation is a `#GP` fault, not a Rust-catchable error.
pub unsafe fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high, options(nostack, preserves_flags));
    }
}

/// Points the TSS's RSP0 field at `rsp0` -- the stack the CPU automatically
/// switches to on any ring3->ring0 transition (a syscall or an exception/
/// interrupt hitting user-mode code). `task.rs`'s scheduler calls this on
/// every context switch, pointing it at whichever task is about to run's
/// own dedicated stack, so that transition always lands somewhere valid
/// instead of at whatever RSP0 was last left at (0, until the first task
/// that actually needs it sets it).
///
/// # Safety
/// Must be called after `init()`. `rsp0` should be the top of a stack that
/// stays valid (allocated, not freed) for as long as it might be used.
pub unsafe fn set_kernel_stack(rsp0: u64) {
    unsafe {
        TSS.rsp[0] = rsp0;
        SYSCALL_KERNEL_RSP = rsp0;
    }
}
