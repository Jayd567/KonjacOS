//! Minimal 4-level page table manager, built *on top of* the page tables
//! Limine already set up (we keep using its CR3 as the template -- there's
//! no reason to build the kernel's own mappings from scratch). Most of
//! this module just walks a set of tables and adds new mappings where none
//! exist yet, allocating new page-table-level frames from `pmm` as needed.
//! Existing mappings (kernel image, framebuffer, HHDM) are left completely
//! alone.
//!
//! [`new_address_space`]/[`map_page_in`]/[`load_cr3`] are what `usermode.rs`
//! uses to give each ring-3 task its own *separate* PML4 instead of every
//! task sharing the one Limine handed us -- see [`new_address_space`]'s doc
//! comment for how the isolation boundary actually works (shared kernel
//! half, private user half).
//!
//! Table frames are accessed through the HHDM, same as `pmm` -- physical
//! address `p` is readable/writable at `hhdm_offset + p`.

use core::arch::global_asm;

use crate::pmm;

pub const PAGE_PRESENT: u64 = 1 << 0;
pub const PAGE_WRITABLE: u64 = 1 << 1;
/// Lets CPL=3 code reach this page at all -- without it, ring 3 faults the
/// instant it touches the mapping, present or not. x86 requires this bit
/// set at *every* level (PML4/PDPT/PD/PT) for a page to actually be
/// user-reachable, which is why `ensure_table` below sets it unconditionally
/// on every intermediate table it creates: that alone grants nothing, since
/// each individual leaf page is still separately gated by its own PTE flags
/// -- it just avoids the alternative of tracking "does anything under this
/// table need to be user-visible" and retrofitting parent entries later.
pub const PAGE_USER: u64 = 1 << 2;
/// Bit 63 of a PTE: "instruction fetches from this page raise a #PF" --
/// the same fault vector every other protection violation this kernel
/// already handles takes, distinguishable only by bit 4 ("instruction
/// fetch") of the CPU's #PF error code, which nothing here currently
/// inspects separately -- an execute violation just falls through the
/// same fatal/`SIGSEGV`-delivery path a write-to-read-only or access-to-
/// `PROT_NONE` fault already does (see item 25/26's README entries).
/// Only meaningful once `EFER.NXE` is actually set (see [`init`]) --
/// before that, this bit is simply reserved-and-ignored by the CPU,
/// which is exactly why every mapping made before item 27 stayed
/// executable regardless of `PAGE_WRITABLE`/`PAGE_USER`: nothing ever
/// set this bit *or* enabled the feature that gives it meaning.
pub const PAGE_NX: u64 = 1 << 63;

const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const ENTRIES_PER_TABLE: usize = 512;

fn hhdm_offset() -> u64 {
    pmm::hhdm_offset()
}

fn table_ptr(phys: u64) -> *mut u64 {
    (hhdm_offset() + phys) as *mut u64
}

/// Reads the current CR3 (physical address of the PML4), masking off the
/// low flag bits CR3 also carries (PCID etc. -- unused here, but mask them
/// out anyway so the result is a clean frame address).
fn read_cr3() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) value);
    }
    value & ADDR_MASK
}

fn invlpg(virt: u64) {
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags));
    }
}

/// The physical address of whichever PML4 is loaded into CR3 right now --
/// i.e. the address space the CPU is currently translating through.
pub fn current_cr3() -> u64 {
    read_cr3()
}

/// Whether this CPU actually supports the NX (execute-disable) feature at
/// all -- `CPUID.80000001H:EDX[20]`, real hardware feature discovery, not
/// an assumption. Every real x86-64 CPU built in the last two decades
/// (and every QEMU CPU model this kernel has ever booted under) reports
/// this bit set, but [`init`] checks it for real rather than joining the
/// long, unglamorous list of kernels that silently assumed a feature was
/// universal and got away with it right up until they didn't. Set once
/// by [`init`]; read by [`nx_available`].
static NX_AVAILABLE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether `EFER.NXE` actually got enabled -- see [`init`]. What
/// `linux_syscall.rs`'s `sys_mprotect` checks before ever setting
/// [`PAGE_NX`] on a real PTE: setting a reserved-and-ignored bit is
/// harmless, but this kernel would rather honestly leave `PROT_EXEC`
/// unenforced (exactly item 26's documented gap) on the one CPU that
/// somehow doesn't support it than pretend a feature is active that
/// isn't.
pub fn nx_available() -> bool {
    NX_AVAILABLE.load(core::sync::atomic::Ordering::Relaxed)
}

/// Enables real NX (execute-disable) page protection -- `EFER.NXE` --
/// after confirming via `CPUID` that this CPU actually implements it.
/// Must run before anything sets [`PAGE_NX`] on a real PTE (nothing did,
/// before item 27); harmless to call before any mappings exist at all,
/// since `PAGE_NX` unset (the state of every existing mapping already
/// made by the time this runs) behaves identically whether or not `NXE`
/// is enabled -- this only changes the meaning of a bit nothing has set
/// yet.
///
/// # Safety
/// Must be called exactly once, early in `kstart`, before `sti` and
/// before anything relies on [`PAGE_NX`] actually being enforced.
pub unsafe fn init() {
    const CPUID_EXT_FEATURES: u32 = 0x8000_0001;
    const EDX_NX_BIT: u32 = 1 << 20;
    const IA32_EFER: u32 = 0xC000_0080;
    const EFER_NXE: u64 = 1 << 11;

    // `cpuid` clobbers `rbx` too, but LLVM reserves `rbx` for its own use
    // and won't let inline asm name it as an operand -- saved/restored by
    // hand around the instruction instead, the standard workaround (same
    // one real-world `cpuid` wrappers in `no_std` Rust use).
    let edx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "pop rbx",
            inout("eax") CPUID_EXT_FEATURES => _,
            out("edx") edx,
            out("ecx") _,
            options(nostack, preserves_flags),
        );
    }
    if edx & EDX_NX_BIT == 0 {
        return; // Genuinely unsupported -- see nx_available's doc comment for what this means downstream.
    }
    NX_AVAILABLE.store(true, core::sync::atomic::Ordering::Relaxed);

    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") IA32_EFER, out("eax") low, out("edx") high, options(nostack, preserves_flags));
    }
    let efer = ((high as u64) << 32) | low as u64;
    unsafe {
        crate::gdt::wrmsr(IA32_EFER, efer | EFER_NXE);
    }
}

/// Switches the CPU to a different address space by loading a new PML4
/// into CR3. Implicitly flushes every non-global TLB entry (that's just
/// what writing CR3 does on x86) -- `task.rs`'s scheduler only calls this
/// when the incoming task's address space actually differs from the
/// outgoing one, since doing it on every switch between kernel threads
/// (which all share one address space) would flush the TLB 100 times a
/// second for no reason.
///
/// # Safety
/// `pml4_phys` must be the physical address of a valid, fully-formed PML4
/// (in particular: its kernel-half entries, indices 256..512, must mirror
/// the ones every other address space uses -- see [`new_address_space`] --
/// or kernel code/data becomes unreachable the instant this runs).
pub unsafe fn load_cr3(pml4_phys: u64) {
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) pml4_phys, options(nostack, preserves_flags));
    }
}

/// Allocates a fresh, otherwise-empty address space for a new isolated
/// (ring-3) task: a brand new PML4 frame whose upper half (indices
/// 256..512, i.e. every canonical address with the sign bit set -- HHDM,
/// the kernel image, the heap, the framebuffer's mapping, all of it) is
/// copied from whichever PML4 is currently active, and whose lower half
/// (every canonical *positive* address -- where `usermode.rs` puts a
/// task's code and stack) starts completely unmapped.
///
/// This is the actual isolation boundary: two tasks each built this way
/// share the exact same kernel mappings (so kernel code always works no
/// matter which task's address space happens to be loaded when an
/// interrupt or syscall lands), but have entirely independent, private
/// low-half page tables -- a page one task maps at some user address is
/// simply not present at all in the other's. Note that only the *PML4
/// entries* are copied, not the tables they point to: since those lower-
/// level PDPT/PD/PT frames are shared by reference, any future kernel-side
/// mapping made through an *already-covered* PML4 slot (e.g. the heap
/// growing within its existing region) is automatically visible to every
/// address space built from this function, no extra bookkeeping needed --
/// only a mapping that needed a *new* top-level slot would require this
/// copy to happen again, which is why `spawn_demo`/callers should only
/// call this after the kernel's own major subsystems (heap, framebuffer,
/// etc.) are already initialized.
///
/// # Safety
/// Must be called with a currently-active, fully-initialized kernel
/// address space (true anywhere after `heap::init`/`memory::init`).
pub unsafe fn new_address_space() -> u64 {
    let new_pml4_phys = pmm::alloc_frame().expect("paging: out of physical memory for a new address space");
    let new_table = table_ptr(new_pml4_phys);
    unsafe {
        core::ptr::write_bytes(new_table as *mut u8, 0, ENTRIES_PER_TABLE * 8);
    }

    let source_table = table_ptr(read_cr3());
    for i in 256..ENTRIES_PER_TABLE {
        unsafe {
            let entry = source_table.add(i).read_volatile();
            new_table.add(i).write_volatile(entry);
        }
    }

    new_pml4_phys
}

/// Frees every physical frame backing an address space's private (user,
/// low) half: every leaf page a task ever mapped via [`map_page_in`], plus
/// every PDPT/PD/PT frame [`ensure_table`] allocated to reach them, plus
/// the PML4 frame itself. Only ever walks indices `0..256` -- the kernel
/// half (`256..512`) points at frames [`new_address_space`] *copied by
/// reference*, shared by every address space that's ever existed, not
/// allocated fresh for this one; recursing into it here would free memory
/// every other task -- including whichever one is about to run next --
/// still depends on. This is the counterpart [`new_address_space`] never
/// had until now: without it, every ring-3 task's private address space
/// (its code/stack/heap frames, and the page-table frames mapping them)
/// just leaked for good the moment the task exited.
///
/// # Safety
/// `pml4_phys` must not be the address space currently loaded into CR3 --
/// freeing memory out from under yourself is undefined behavior the
/// instant the next instruction fetch or stack access needs it -- and
/// nothing else may still hold a reference to anything this address
/// space's private half maps. Both hold for a `Terminated` task that's
/// been fully switched away from for good (see `task.rs`'s reaping sweep,
/// the only caller: it never reaps the *currently running* task's own
/// slot for exactly this reason).
pub unsafe fn destroy_address_space(pml4_phys: u64) {
    crate::vm::destroy(pml4_phys);
    let pml4 = table_ptr(pml4_phys);
    for i in 0..256 {
        let pml4_entry = unsafe { pml4.add(i).read_volatile() };
        if pml4_entry & PAGE_PRESENT == 0 {
            continue;
        }
        let pdpt_phys = pml4_entry & ADDR_MASK;
        let pdpt = table_ptr(pdpt_phys);
        for j in 0..ENTRIES_PER_TABLE {
            let pdpt_entry = unsafe { pdpt.add(j).read_volatile() };
            if pdpt_entry & PAGE_PRESENT == 0 {
                continue;
            }
            let pd_phys = pdpt_entry & ADDR_MASK;
            let pd = table_ptr(pd_phys);
            for k in 0..ENTRIES_PER_TABLE {
                let pd_entry = unsafe { pd.add(k).read_volatile() };
                if pd_entry & PAGE_PRESENT == 0 {
                    continue;
                }
                let pt_phys = pd_entry & ADDR_MASK;
                let pt = table_ptr(pt_phys);
                for l in 0..ENTRIES_PER_TABLE {
                    let pt_entry = unsafe { pt.add(l).read_volatile() };
                    if pt_entry & PAGE_PRESENT == 0 {
                        continue;
                    }
                    unsafe { pmm::free_frame(pt_entry & ADDR_MASK) };
                }
                unsafe { pmm::free_frame(pt_phys) };
            }
            unsafe { pmm::free_frame(pd_phys) };
        }
        unsafe { pmm::free_frame(pdpt_phys) };
    }
    unsafe { pmm::free_frame(pml4_phys) };
}

/// Splits a canonical virtual address into its four 9-bit page-table
/// indices (PML4, PDPT, PD, PT), most-significant first.
fn indices(virt: u64) -> [usize; 4] {
    [
        ((virt >> 39) & 0x1FF) as usize,
        ((virt >> 30) & 0x1FF) as usize,
        ((virt >> 21) & 0x1FF) as usize,
        ((virt >> 12) & 0x1FF) as usize,
    ]
}

/// Returns the physical address of the next-level table referenced by
/// `table[index]`, allocating and zeroing a fresh frame (and wiring it into
/// `table[index]` as present+writable) if none exists yet.
///
/// # Safety
/// `table_phys` must be a valid, currently-active (or at least
/// HHDM-accessible) page-table frame, and `index` must be < 512.
unsafe fn ensure_table(table_phys: u64, index: usize) -> u64 {
    let table = table_ptr(table_phys);
    let entry = unsafe { table.add(index).read_volatile() };
    if entry & PAGE_PRESENT != 0 {
        return entry & ADDR_MASK;
    }

    let new_frame = pmm::alloc_frame().expect("paging: out of physical memory for a page table");
    let new_table = table_ptr(new_frame);
    unsafe {
        core::ptr::write_bytes(new_table as *mut u8, 0, ENTRIES_PER_TABLE * 8);
        table.add(index).write_volatile(new_frame | PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
    }
    new_frame
}

/// Maps one 4 KiB page at virtual address `virt` to physical frame `phys`
/// in the **currently active** address space (both must already be 4 KiB
/// aligned) with the given extra flags (`PAGE_WRITABLE` etc. --
/// `PAGE_PRESENT` is always added). Intermediate PML4/PDPT/PD entries are
/// created as needed. Overwrites any existing mapping for `virt`.
///
/// # Safety
/// The caller must ensure this mapping doesn't alias physical memory
/// that's still owned/mapped elsewhere in a conflicting way, and that
/// `virt`/`phys` are actually page-aligned.
pub unsafe fn map_page(virt: u64, phys: u64, flags: u64) {
    unsafe { map_page_in(read_cr3(), virt, phys, flags) };
}

/// Same as [`map_page`], but targets an arbitrary address space (any PML4
/// physical address, not necessarily the one currently loaded into CR3) --
/// what `usermode.rs` uses to build up a new task's private mappings via
/// [`new_address_space`] *before* that address space is ever switched to.
/// Reachable through the HHDM the same way every other page-table frame
/// is, so this works regardless of whether `pml4_phys` is "active".
///
/// # Safety
/// Same requirements as [`map_page`], plus: `pml4_phys` must be a valid
/// PML4 frame (typically one [`new_address_space`] just returned).
pub unsafe fn map_page_in(pml4_phys: u64, virt: u64, phys: u64, flags: u64) {
    debug_assert!(virt & 0xFFF == 0, "paging::map_page_in: virt not page-aligned");
    debug_assert!(phys & 0xFFF == 0, "paging::map_page_in: phys not page-aligned");

    let idx = indices(virt);

    unsafe {
        let pdpt_phys = ensure_table(pml4_phys, idx[0]);
        let pd_phys = ensure_table(pdpt_phys, idx[1]);
        let pt_phys = ensure_table(pd_phys, idx[2]);

        let pt = table_ptr(pt_phys);
        pt.add(idx[3]).write_volatile(phys | PAGE_PRESENT | flags);
    }

    // Only meaningful if `pml4_phys` happens to be the active address
    // space -- harmless (just a redundant local flush) otherwise, since
    // invlpg only ever drops whatever entry the current CPU has cached
    // for this virtual address, regardless of which address space it
    // originally came from.
    invlpg(virt);
}

/// Fallible installation for a previously absent demand page. On failure,
/// undo every intermediate table allocated by this call; caller owns `phys`.
/// # Safety
/// Same address-space/alignment requirements as map_page_in. IRQs must be off
/// and no concurrent editor may change these page tables while this runs.
pub unsafe fn try_map_page_in(pml4_phys: u64, virt: u64, phys: u64, flags: u64) -> bool {
    let idx = indices(virt);
    let mut table = pml4_phys;
    let mut created = [(0u64, 0usize, 0u64); 3];
    let mut count = 0;
    for level in 0..3 {
        let entry = unsafe { table_ptr(table).add(idx[level]).read_volatile() };
        if entry & PAGE_PRESENT != 0 {
            // User mappings here are 4 KiB; never descend through a huge page.
            if entry & (1 << 7) != 0 { return false; }
            table = entry & ADDR_MASK;
        } else if let Some(next) = pmm::alloc_frame() {
            unsafe {
                core::ptr::write_bytes(table_ptr(next) as *mut u8, 0, 4096);
                table_ptr(table).add(idx[level]).write_volatile(next | PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
            }
            created[count] = (table, idx[level], next);
            count += 1;
            table = next;
        } else {
            for &(parent, index, frame) in created[..count].iter().rev() {
                unsafe { table_ptr(parent).add(index).write_volatile(0); pmm::free_frame(frame); }
            }
            return false;
        }
    }
    let leaf = unsafe { table_ptr(table).add(idx[3]) };
    if unsafe { leaf.read_volatile() } & PAGE_PRESENT != 0 { return false; }
    unsafe { leaf.write_volatile(phys | PAGE_PRESENT | flags) };
    invlpg(virt);
    true
}

/// The exact stack layout `isr_stub_14` leaves behind after its 15 GPR
/// pushes: `r15` (the last one pushed) sits at the lowest address, so it's
/// this struct's first field, all the way up through `rax`, then the
/// CPU's own real #PF error code, then the CPU-pushed `iretq` frame
/// (`RIP`/`CS`/`RFLAGS`/`RSP`/`SS` -- always all five, since a #PF from
/// ring 3 is always a privilege-changing transition). `#[repr(C)]` with
/// fields declared in that exact ascending-address order makes this a
/// direct, zero-copy view onto the live stack -- writing a field through
/// `&mut PfFrame` writes the literal memory `iretq` will read back, the
/// same trick `handle_page_fault`'s resume path already relied on
/// implicitly before this struct existed, just now named and safe to
/// index by field instead of by raw offset.
#[repr(C)]
struct PfFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    error_code: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

/// Backs `syscall.rs`'s `SYS_MMAP`: called by `idt.rs`'s dedicated #PF
/// (vector 14) stub for *every* page fault, with a pointer at the live
/// GPR/iretq-frame stack (see [`PfFrame`]) and CR2 (the faulting virtual
/// address). Returns nonzero for two different cases, both of which mean
/// "the frame has already been adjusted so `isr_stub_14`'s existing
/// resume path (fxrstor, pop GPRs, iretq) can just run unmodified":
///
/// 1. A fault at an address the currently running task has reserved via
///    `SYS_MMAP` (tracked by `vm`) but never actually
///    touched before -- a physical frame has already been allocated,
///    filled from the file or zeroed, and mapped with the requested protection, and the faulting instruction can safely retry
///    right where it was.
/// 2. A genuinely fatal fault (null pointer, write to read-only memory,
///    unreserved memory, ...) in a ring-3 task that has a real `SIGSEGV`
///    handler installed via `rt_sigaction` -- see [`try_deliver_sigsegv`].
///    The frame's `RIP`/`RSP`/`RDI` have been redirected to the handler
///    instead of the original faulting instruction.
///
/// Returns `0` for everything else -- an out-of-memory condition partway
/// through servicing an otherwise-legitimate case-1 fault, a fault with
/// no case-2 handler to go to, or a fault that isn't ring 3 at all (a
/// real kernel bug). Those all fall through to the same fatal
/// `exception_handler` path every other CPU exception already uses --
/// this function only ever *adds* recovery paths, it doesn't remove the
/// existing one.
///
/// This is deliberately narrow: real demand paging also covers file-backed
/// mappings, copy-on-write, and swap, none of which exist here. What's
/// here is exactly enough for anonymous `SYS_MMAP` memory (what a real
/// allocator/GC leans on) to work without eagerly mapping every byte a
/// program *might* touch up front, the same "reserve now, back on first
/// touch" idea real operating systems use, just without the extra
/// machinery those don't need to prove out first.
///
/// # Safety
/// Must only be called from the #PF stub, with the faulting task's own
/// address space still active (true by construction: a page fault can't
/// switch CR3 out from under itself) and interrupts still disabled (true
/// inside any interrupt-gate handler, this one included). `frame` must
/// point at a live `isr_stub_14` stack frame, exactly as `PfFrame`
/// describes.
#[unsafe(no_mangle)]
extern "C" fn handle_page_fault(frame: *mut PfFrame, fault_addr: u64) -> u64 {
    let f = unsafe { &mut *frame };

    // Bit 0 of a #PF's error code is the "was the page present" flag --
    // set means the page *was* mapped and the access still faulted (wrong
    // permissions, wrong privilege level, etc.), which is never something
    // a not-yet-backed SYS_MMAP reservation is responsible for. Only a
    // not-present fault (bit clear) is a candidate for "first touch of
    // reserved-but-unbacked memory."
    const PRESENT: u64 = 1 << 0;
    if f.error_code & PRESENT == 0 && crate::vm::fault(fault_addr, f.error_code) {
        return 1;
    }

    // Not a recoverable lazy-mmap fault. `CS`'s low 2 bits are the RPL the
    // CPU was running at when it faulted -- 3 means this was genuinely
    // ring-3 code (never a kernel bug this recovery path should paper
    // over), the only case real `SIGSEGV` delivery is ever appropriate.
    if f.cs & 3 == 3 && try_deliver_sigsegv(f) {
        return 1;
    }

    0
}

/// Real `SIGSEGV` delivery for a fatal ring-3 page fault -- see item 25's
/// README entry for the full design (why this doesn't need to build a
/// byte-perfect Linux `ucontext_t`, how `rt_sigreturn` gets back to the
/// original context, ...). Mutates `f` in place (new `RIP`/`RSP`/`RDI`)
/// and returns `true` only when delivery actually happened; `false` means
/// "leave `f` untouched, this has to be the ordinary fatal path" --
/// checked in this order:
///
/// - No handler installed for `SIGSEGV` at all (`handler <= 1`: `0` is
///   `SIG_DFL`, `1` is `SIG_IGN` -- neither is a real address to jump to,
///   and real Linux's own default+ignore actions for `SIGSEGV` are "kill
///   the process"/"kill the process anyway" respectively, not "resume,"
///   so there's nothing more lenient this could do for either case).
/// - No `SA_RESTORER`/`sa_restorer` (required on x86-64 by every real
///   libc that installs a handler here; without one this kernel has no
///   trampoline to hand control back through after the handler returns,
///   so it honestly can't deliver one rather than silently mishandling
///   it).
/// - This task is already inside a `SIGSEGV` handler
///   (`task::is_delivering_signal`) -- real Linux blocks a signal for the
///   duration of its own (non-`SA_NODEFER`) handler, so a fault that
///   recurs there kills the process instead of recursing forever; this is
///   the same safety net, not a limitation unique to this kernel.
fn try_deliver_sigsegv(f: &mut PfFrame) -> bool {
    const SIGSEGV: u8 = 11;
    const SA_RESTORER: u64 = 0x0400_0000;

    if crate::task::is_delivering_signal(SIGSEGV) {
        return false;
    }
    let (handler, flags, restorer) = crate::task::sigaction(SIGSEGV as u64);
    if handler <= 1 || flags & SA_RESTORER == 0 || restorer == 0 {
        return false;
    }

    let ctx = crate::task::SavedContext {
        rip: f.rip,
        cs: f.cs,
        rflags: f.rflags,
        rsp: f.rsp,
        ss: f.ss,
        rax: f.rax,
        rbx: f.rbx,
        rcx: f.rcx,
        rdx: f.rdx,
        rsi: f.rsi,
        rdi: f.rdi,
        rbp: f.rbp,
        r8: f.r8,
        r9: f.r9,
        r10: f.r10,
        r11: f.r11,
        r12: f.r12,
        r13: f.r13,
        r14: f.r14,
        r15: f.r15,
    };
    if !crate::task::begin_signal_delivery(SIGSEGV, ctx) {
        return false;
    }

    // Redirect execution to the handler exactly the way a real `call`
    // would have, minus the actual `call` instruction: push the return
    // address (the real `sa_restorer` musl/glibc already gave us --
    // trusted the same limited way every other guest-supplied pointer in
    // this kernel is) onto a *new* stack pointer, a real gap below the
    // original RSP so a handler that hasn't touched the stack yet doesn't
    // silently clobber the interrupted context's still-live red zone
    // (the x86-64 SysV ABI's 128-byte "leaf function may use below RSP
    // without adjusting it" allowance) before it's even been read.
    let mut new_rsp = f.rsp.wrapping_sub(512) & !0xF;
    new_rsp = new_rsp.wrapping_sub(8);
    unsafe {
        (new_rsp as *mut u64).write(restorer);
    }
    f.rsp = new_rsp;
    f.rip = handler;
    f.rdi = SIGSEGV as u64; // the one argument a plain `void handler(int)` expects -- see the module docs' "no SA_SIGINFO yet" scope note.
    true
}

// Entered via a genuine CPU exception (#PF, vector 14), not `int 0x80` --
// otherwise the same shape as syscall.rs's syscall_stub: full GPR save,
// an aligned stack-local fxsave/fxrstor buffer (a nested fault must preserve
// an outer syscall's saved user state), and a real Rust function call, because
// unlike idt.rs's other exception stubs (which only ever print-and-halt,
// so they never need to resume anything), a page fault that
// handle_page_fault successfully services needs to retry the faulting
// instruction via iretq exactly as if nothing happened. Only falls back
// to the shared fatal exception_handler path when handle_page_fault
// returns 0 -- a real bug, not a recoverable lazy-mapping fault.
global_asm!(
    r#"
.section .text

.global isr_stub_14
isr_stub_14:
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

    # rsp already points exactly at PfFrame's first field (r15, the last
    # of the 15 GPRs pushed above) -- see that struct's doc comment for
    # why handle_page_fault can index it by name instead of this stub
    # having to hand over each piece separately the way it used to.
    mov rdi, rsp
    mov rsi, cr2
    # The CPU frame plus error code and 15 GPRs does not satisfy the
    # SysV call-site alignment. Preserve the exact frame in a callee-saved
    # register, then allocate an aligned, nesting-safe FPU save area.
    # A PF inside a syscall must not overwrite that syscall's user save.
    mov rbx, rsp
    and rsp, -16
    sub rsp, 512
    fxsave [rsp]
    call handle_page_fault
    fxrstor [rsp]
    mov rsp, rbx
    test rax, rax
    jz .Lpf_fatal

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
    add rsp, 8      # discard the CPU-pushed error code before iretq
    iretq

.Lpf_fatal:
    # Not recoverable -- fall through to the same fatal path every other
    # CPU exception already uses. No need to restore GPRs first:
    # exception_handler is `-> !`, it never returns.
    mov rdi, 14
    mov rsi, [rsp + 15*8]
    mov rdx, [rsp + 16*8]
    mov rcx, [rsp + 17*8]
    mov r8, rsp
    mov r9, [rsp + 19*8]
    and rsp, -16
    call exception_handler
    cli
.Lpf_halt:
    hlt
    jmp .Lpf_halt
"#
);

/// Looks up the physical frame currently mapped for `virt` within
/// `pml4_phys`'s address space, without disturbing anything -- a
/// non-destructive walk (PML4 -> PDPT -> PD -> PT), same shape as
/// [`destroy_address_space`]'s walk minus the freeing. Returns `None` if
/// any level along the way is missing (page not present). What
/// `loader.rs` uses to apply a PIE's self-relocations after mapping its
/// segments: it needs to write a fixed-up value into memory it already
/// mapped, addressed by *virtual* address, but the only way to reach a
/// mapped physical frame from kernel code is through the HHDM, which
/// needs the *physical* address -- this bridges that gap.
///
/// # Safety
/// `pml4_phys` must be a valid PML4 frame.
pub unsafe fn translate(pml4_phys: u64, virt: u64) -> Option<u64> {
    let idx = indices(virt);
    unsafe {
        let pml4 = table_ptr(pml4_phys);
        let pml4_entry = pml4.add(idx[0]).read_volatile();
        if pml4_entry & PAGE_PRESENT == 0 {
            return None;
        }
        let pdpt = table_ptr(pml4_entry & ADDR_MASK);
        let pdpt_entry = pdpt.add(idx[1]).read_volatile();
        if pdpt_entry & PAGE_PRESENT == 0 {
            return None;
        }
        let pd = table_ptr(pdpt_entry & ADDR_MASK);
        let pd_entry = pd.add(idx[2]).read_volatile();
        if pd_entry & PAGE_PRESENT == 0 {
            return None;
        }
        let pt = table_ptr(pd_entry & ADDR_MASK);
        let pt_entry = pt.add(idx[3]).read_volatile();
        if pt_entry & PAGE_PRESENT == 0 {
            return None;
        }
        Some(pt_entry & ADDR_MASK)
    }
}

/// Clears whatever mapping `virt` has in `pml4_phys`'s address space
/// (same walk as [`translate`], but writes a zeroed entry back into the
/// PT instead of just reading it) and flushes that one address out of the
/// TLB with `invlpg` -- otherwise a stale translation could keep
/// answering reads/writes against a physical frame the caller is about to
/// hand back to `pmm` as free, exactly the kind of bug that "looks fine
/// until something else reuses that frame" and corrupts unrelated memory.
/// Returns the physical frame that *was* mapped there, so a caller (real
/// `munmap`, below, or `linux_syscall.rs`'s `MAP_FIXED`/`PROT_NONE` guard-
/// page handling) can free it -- or `None` if `virt` wasn't mapped at all,
/// which is a perfectly ordinary outcome (an `mmap`'d-but-never-touched,
/// still-lazily-backed page, say), not an error.
///
/// Unlike [`destroy_address_space`], this only ever clears one leaf PTE --
/// it deliberately never frees an emptied PDPT/PD/PT frame itself, even if
/// this was the last present entry in it. That's a real, intentional gap
/// (a long-running process that `mmap`s and `munmap`s many separate
/// regions leaks page-table frames, not just data frames), not an
/// oversight: reclaiming an emptied intermediate table safely means
/// checking all 512 of its siblings are also empty first, which is real
/// extra work `destroy_address_space` never had to do (it always frees
/// every level, unconditionally, because the whole address space is going
/// away at once) -- left for later, same honest-gap spirit as `syscall.rs`'s
/// `mmap` module docs.
///
/// # Safety
/// `pml4_phys` must be a valid PML4 frame, and `virt` must be 4 KiB
/// aligned. If `pml4_phys` is the currently active address space, the
/// `invlpg` here is what makes this immediately safe to reuse the
/// returned frame for something else; if it's some *other* task's address
/// space, there's no stale-TLB risk in the first place (nothing has it
/// loaded into CR3), so the `invlpg` is simply harmless.
pub unsafe fn unmap_page(pml4_phys: u64, virt: u64) -> Option<u64> {
    let idx = indices(virt);
    let phys = unsafe {
        let pml4 = table_ptr(pml4_phys);
        let pml4_entry = pml4.add(idx[0]).read_volatile();
        if pml4_entry & PAGE_PRESENT == 0 {
            return None;
        }
        let pdpt = table_ptr(pml4_entry & ADDR_MASK);
        let pdpt_entry = pdpt.add(idx[1]).read_volatile();
        if pdpt_entry & PAGE_PRESENT == 0 {
            return None;
        }
        let pd = table_ptr(pdpt_entry & ADDR_MASK);
        let pd_entry = pd.add(idx[2]).read_volatile();
        if pd_entry & PAGE_PRESENT == 0 {
            return None;
        }
        let pt = table_ptr(pd_entry & ADDR_MASK);
        let pt_entry = pt.add(idx[3]).read_volatile();
        if pt_entry & PAGE_PRESENT == 0 {
            return None;
        }
        pt.add(idx[3]).write_volatile(0);
        pt_entry & ADDR_MASK
    };
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags));
    }
    Some(phys)
}

/// Real `mprotect(2)`: rewrites the PTE flags of every already-mapped
/// page in `[virt, virt + len)` within `pml4_phys`'s address space,
/// preserving each one's existing physical mapping -- same walk as
/// [`translate`], but writes `phys | PAGE_PRESENT | flags` back instead
/// of just reading. `virt` is rounded down and the range rounded up to a
/// whole number of pages, same "be lenient about an unaligned length,
/// same as a real kernel" spirit `SYS_MMAP` already practices.
///
/// Absent pages are skipped: vm::protect updates reservation metadata first,
/// so future faults apply the requested protection without allocating pages.
///
/// # Safety
/// `pml4_phys` must be a valid PML4 frame. If it's the currently active
/// address space, `invlpg` makes each updated mapping's new permissions
/// visible immediately, same as every other flag-changing function here;
/// if it's some other task's, there's no stale-TLB risk to begin with.
pub unsafe fn protect_range_in(pml4_phys: u64, virt: u64, len: u64, flags: u64) {
    let start = virt & !0xFFF;
    let end = (virt + len + 0xFFF) & !0xFFF;
    let mut addr = start;
    while addr < end {
        let idx = indices(addr);
        unsafe {
            let pml4 = table_ptr(pml4_phys);
            let pml4_entry = pml4.add(idx[0]).read_volatile();
            if pml4_entry & PAGE_PRESENT == 0 {
                addr += 4096;
                continue;
            }
            let pdpt = table_ptr(pml4_entry & ADDR_MASK);
            let pdpt_entry = pdpt.add(idx[1]).read_volatile();
            if pdpt_entry & PAGE_PRESENT == 0 {
                addr += 4096;
                continue;
            }
            let pd = table_ptr(pdpt_entry & ADDR_MASK);
            let pd_entry = pd.add(idx[2]).read_volatile();
            if pd_entry & PAGE_PRESENT == 0 {
                addr += 4096;
                continue;
            }
            let pt = table_ptr(pd_entry & ADDR_MASK);
            let pt_entry = pt.add(idx[3]).read_volatile();
            if pt_entry & PAGE_PRESENT == 0 {
                addr += 4096;
                continue;
            }
            let phys = pt_entry & ADDR_MASK;
            pt.add(idx[3]).write_volatile(phys | PAGE_PRESENT | flags);
        }
        invlpg(addr);
        addr += 4096;
    }
}
