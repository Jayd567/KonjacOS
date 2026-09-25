//! Sets up KonjacOS's ring-3 demo tasks: tiny, hand-written machine-code
//! programs that prove user mode actually works by making two `int 0x80`
//! syscalls (see `syscall.rs`) -- one to print a message, one to exit
//! cleanly.
//!
//! There's no ELF loader yet, so "loading" this program means copying its
//! bytes (assembled straight into the kernel binary, via the `global_asm!`
//! block below) to freshly allocated, freshly `PAGE_USER`-mapped pages and
//! pointing a new task at the copy -- deliberately *not* executing it in
//! place from the kernel's own `.text`, which Limine mapped for ring 0 only
//! and has no `PAGE_USER` bit set anywhere; ring 3 fetching from it would
//! just page-fault immediately. The program is written to be position-
//! independent (RIP-relative addressing throughout) specifically so that
//! copying it elsewhere and running it from there works correctly.
//!
//! `spawn_demo` builds **two** of these, each in its own private address
//! space (`paging::new_address_space`), both mapped at the exact same
//! virtual addresses (`USER_CODE_BASE`/`USER_STACK_BASE`). That's not a
//! coincidence: it's the actual proof that process isolation works.
//! Before per-task page tables existed, running two tasks that both wanted
//! that address would have been a straight-up conflict -- the second
//! `map_page` call would silently repoint the *one* shared PTE at its own
//! frame, corrupting whichever task was still using the first one. Now
//! each task's mapping lives in its own PML4, so both run correctly, fully
//! unaware of each other, despite sharing what looks like the same address.

extern crate alloc;

use crate::paging::{self, PAGE_USER, PAGE_WRITABLE};
use crate::pmm;
use crate::task;

/// A low, canonical address -- the kind of address real "user space" uses,
/// as opposed to this kernel's usual 0xffff... addresses. Every isolated
/// task maps its code here, in its own private address space; see the
/// module docs for why that's safe now instead of a collision.
const USER_CODE_BASE: u64 = 0x0000_0040_0000_0000;
const USER_STACK_BASE: u64 = 0x0000_0050_0000_0000;
const USER_STACK_SIZE: u64 = 16 * 1024;
const PAGE_SIZE: u64 = 4096;

unsafe extern "C" {
    static user_program_start: u8;
    static user_program_end: u8;
}

/// # Safety
/// Must run after `task::init()` (needs the scheduler ready) and after
/// `pmm`/`paging`/`heap` are ready (true from very early boot on, but see
/// `paging::new_address_space`'s own safety note about ordering relative
/// to the kernel's major subsystems).
pub unsafe fn spawn_demo() {
    unsafe {
        spawn_one("ring3-demo-a");
        spawn_one("ring3-demo-b");
    }
}

/// Builds one isolated ring-3 task running the demo program, entirely in
/// its own fresh address space.
///
/// # Safety
/// Same requirements as [`spawn_demo`].
unsafe fn spawn_one(name: &'static str) {
    let start = core::ptr::addr_of!(user_program_start) as u64;
    let end = core::ptr::addr_of!(user_program_end) as u64;
    let len = (end - start) as usize;
    let pages = (len as u64).div_ceil(PAGE_SIZE).max(1);

    // A fresh PML4: shares every kernel mapping with the address space
    // that's currently active (whatever's true of every kernel thread,
    // since this always runs from one), starts with a completely empty
    // low half. Nothing below is visible to any other task's address
    // space, and nothing any other task has mapped at these same
    // addresses is visible here.
    let pml4 = unsafe { paging::new_address_space() };
    let hhdm = pmm::hhdm_offset();

    let mut remaining = len;
    let mut src = start;
    for i in 0..pages {
        let virt = USER_CODE_BASE + i * PAGE_SIZE;
        let phys = pmm::alloc_frame().expect("usermode: out of memory mapping the demo program");
        unsafe {
            paging::map_page_in(pml4, virt, phys, PAGE_WRITABLE | PAGE_USER);
        }

        // This new mapping isn't necessarily (and for every task after the
        // first, definitely isn't) in the *currently active* address
        // space, so the program bytes can't be copied through the virtual
        // address the way a single-address-space kernel could get away
        // with -- they go through the physical frame's own always-valid
        // HHDM alias instead.
        let chunk = remaining.min(PAGE_SIZE as usize);
        unsafe {
            core::ptr::copy_nonoverlapping(src as *const u8, (hhdm + phys) as *mut u8, chunk);
        }
        src += chunk as u64;
        remaining -= chunk;
    }

    let stack_pages = USER_STACK_SIZE / PAGE_SIZE;
    for i in 0..stack_pages {
        let virt = USER_STACK_BASE + i * PAGE_SIZE;
        let phys = pmm::alloc_frame().expect("usermode: out of memory mapping the demo user stack");
        unsafe {
            paging::map_page_in(pml4, virt, phys, PAGE_WRITABLE | PAGE_USER);
        }
    }
    let stack_top = USER_STACK_BASE + USER_STACK_SIZE;

    task::spawn_user(name, alloc::string::String::new(), USER_CODE_BASE, stack_top, pml4);
}

// The demo program itself: two syscalls (write, then exit) and nothing
// else. Written by hand rather than compiled from Rust since there's no
// toolchain support here for producing a truly freestanding, relocatable
// user binary yet -- this is a placeholder for a real loader, not a
// long-term way to write user programs.
core::arch::global_asm!(
    r#"
.section .text
.align 16
.global user_program_start
user_program_start:
    lea rdi, [rip + user_msg]
    # Not `mov rsi, user_msg_len` with `user_msg_len` a `.set`/`=` absolute
    # symbol: found the hard way that GNU as's Intel-syntax mode reads a
    # bare symbol operand there as a *memory* reference (`mov rsi,
    # [addr]`), not the symbol's own value -- it assembled to a load from
    # address 0x39 (the message's actual length, misread as a pointer),
    # which is exactly the CR2 this crashed with the first time. Loading it
    # from an explicit quadword removes the ambiguity instead of relying on
    # a hardcoded, easy-to-forget-to-update length.
    mov rsi, [rip + user_msg_len]
    mov rax, 1                # SYS_WRITE
    int 0x80

    mov rax, 0                # SYS_EXIT
    int 0x80
user_hang:
    jmp user_hang              # Unreachable (exit doesn't return), but a
                                # safety net beats running off into whatever
                                # memory comes next if it somehow did.
user_msg:
    .ascii "Hello from ring 3! CPL=3 user-mode syscalls are working.\n"
user_msg_end:
user_msg_len:
    .quad user_msg_end - user_msg
.global user_program_end
user_program_end:
"#
);
