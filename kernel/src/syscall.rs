//! The `int 0x80` syscall gate -- the only sanctioned way out of ring 3
//! short of a CPU exception. Eight syscalls exist so far, deliberately
//! numbered and registered like their Linux namesakes for familiarity even
//! though nothing else about the ABI matches them: 0 = exit, 1 = write(ptr,
//! len), 2 = open(path_ptr, path_len), 3 = read(fd, buf_ptr, len),
//! 4 = close(fd), 5 = brk(new_top), 6 = clone(entry_virt, stack_top),
//! 7 = mmap(len). Arguments come in like the real Linux x86-64 `syscall`
//! convention -- number in `rax`, then `rdi`, `rsi`, `rdx`, `r10` -- even
//! though the actual gate instruction here is `int 0x80`, not `syscall`;
//! there's no reason to invent a fourth calling convention when borrowing
//! a well-known one costs nothing and stays familiar to anyone who's
//! written a raw syscall before.
//!
//! `mmap` is deliberately just the anonymous case -- no file backing, no
//! `MAP_SHARED`, no real flags argument, no `munmap`. It reserves `len`
//! bytes (rounded up to whole pages) in the calling address space's mapping table and hands back the base address, but doesn't map a single
//! byte of physical memory itself: see `paging::handle_page_fault` and
//! `vm::fault` for what actually backs the memory,
//! lazily, the first time anything touches it. That's a real, working
//! demand-paging path, just scoped to exactly the case a program's own
//! heap allocator needs (get some fresh, zeroed, growable memory) rather
//! than the full generality `mmap(2)` offers on a real Unix.
//!
//! `clone` is the odd one out among these seven: every other syscall acts
//! on state that already belongs to the calling task, but `clone` creates
//! a *new* task (`task::spawn_thread`) that shares the caller's address
//! space instead of getting its own -- a thread, not a process. See
//! `task::spawn_thread`'s doc comment for what that actually means and
//! the honest caveats it comes with (heap and open files aren't really
//! shared between siblings yet, just independently snapshotted/empty).
//! The caller is trusted to have already mapped a valid stack for the new
//! thread before asking for it to run -- same trust model as every other
//! pointer argument below.
//!
//! `open`/`read`/`close` are deliberately *not* a general-purpose,
//! Unix-shaped VFS layer: native tasks start with independent descriptor
//! tables; Linux CLONE_FILES threads retain a shared table. An fd is an
//! index into the current table (`task::with_current_open_files`).
//! `open` retains a small FAT16 handle,
//! and `read` streams into a bounded resident kernel buffer. This native ABI
//! has no `write`-to-a-file, `seek`, or directory support. It's exactly
//! enough for a ring-3 program to read a real file off the FAT16 disk,
//! which is the actual point -- see `loader.rs`'s module docs for the
//! broader reasoning about scoping an ABI honestly instead of chasing full
//! compatibility with someone else's.
//!
//! `brk` is similarly minimal: grow-only, page granularity, one fixed
//! region per task (see [`task::USER_HEAP_BASE`]) -- no `mmap`, no
//! unmapping, no per-region protection flags. Enough for a ring-3 program
//! to get a real, dynamically-sized buffer instead of only whatever fits
//! in its statically-sized segments.
//!
//! Every pointer a syscall receives (`write`'s `ptr`, `open`'s `path_ptr`,
//! `read`'s `buf_ptr`) is trusted, not validated against the calling
//! task's own mappings -- a bogus pointer can make the kernel read or write
//! wherever it happens to point, up to `MAX_WRITE_LEN`/`MAX_PATH_LEN` bytes.
//! This *happens* to be safe today only because a ring-3 task's own
//! mappings are the only thing it has any reason to pass in, not because
//! anything here checks -- the real fix (walking the calling task's page
//! tables to confirm every byte of `[ptr, ptr+len)` is actually mapped
//! `PAGE_USER` before touching it) is real process-isolation work, not
//! this pass's job.

use core::arch::global_asm;

use crate::fat16;
use crate::idt;
use crate::paging;
use crate::pmm;
use crate::print;
use crate::println;
use crate::task;
use crate::task::{OpenFile, FileBacking};

const SYSCALL_VECTOR: usize = 0x80;

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_OPEN: u64 = 2;
const SYS_READ: u64 = 3;
const SYS_CLOSE: u64 = 4;
const SYS_BRK: u64 = 5;
const SYS_CLONE: u64 = 6;
const SYS_MMAP: u64 = 7;

/// What every "this failed" return value looks like: `u64::MAX`, i.e. -1
/// reinterpreted as unsigned -- the same trick real syscall ABIs use so a
/// single register can carry either a non-negative result or an error,
/// with the caller responsible for treating the return value as signed.
const SYS_ERROR: u64 = u64::MAX;

/// A generous but firm cap on a single `write`, so a bogus/malicious length
/// can't make the kernel walk off into unmapped memory hunting for bytes
/// that aren't there.
const MAX_WRITE_LEN: usize = 4096;

/// Same idea as `MAX_WRITE_LEN`, for `open`'s path argument.
const MAX_PATH_LEN: usize = 256;

const PAGE_SIZE: u64 = 4096;

/// # Safety
/// Must be called once, after `idt::init()`, and before `sti`.
pub unsafe fn init() {
    unsafe extern "C" {
        fn syscall_stub();
    }
    unsafe {
        idt::set_user_handler(SYSCALL_VECTOR, syscall_stub as *const () as u64);
    }
}

/// Called by `syscall_stub` for every `int 0x80`. Returns a value that ends
/// up back in the caller's `rax`.
#[unsafe(no_mangle)]
extern "C" fn syscall_handler(number: u64, arg0: u64, arg1: u64, arg2: u64, _arg3: u64) -> u64 {
    // `_arg3` isn't read by any of today's six syscalls (the gate still
    // fully plumbs it through from the caller's r10 -- see the module docs
    // -- for whichever future syscall is the first to need a 4th
    // argument, e.g. an `mmap`-style flags word).
    match number {
        SYS_WRITE => {
            let ptr = arg0 as *const u8;
            let len = (arg1 as usize).min(MAX_WRITE_LEN);
            // Safety: see the module docs -- trusted, not validated.
            let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
            match core::str::from_utf8(bytes) {
                Ok(text) => print!("{text}"),
                Err(_) => println!("write: not valid UTF-8 ({len} bytes)"),
            }
            0
        }
        SYS_OPEN => {
            let path_ptr = arg0 as *const u8;
            let path_len = (arg1 as usize).min(MAX_PATH_LEN);
            let bytes = unsafe { core::slice::from_raw_parts(path_ptr, path_len) };
            let path = match core::str::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => return SYS_ERROR,
            };
            let ino = task::hash_path(path);
            match fat16::open_file(path).map(FileBacking::Disk) {
                Ok(data) => task::with_current_open_files(|table| match table.iter().position(|f| f.is_none()) {
                    Some(fd) => {
                        table[fd] = Some(OpenFile { data, pos: 0, ino, extra: None });
                        fd as u64
                    }
                    None => SYS_ERROR, // every fd slot is in use
                }),
                Err(_) => SYS_ERROR, // no such file (or a directory, or an fs error)
            }
        }
        SYS_READ => {
            let fd = arg0 as usize;
            let buf_ptr = arg1 as *mut u8;
            let len = (arg2 as usize).min(MAX_WRITE_LEN);
            let mut buffer = [0u8; MAX_WRITE_LEN];
            match task::read_open_file(fd, &mut buffer[..len], None) {
                Ok(n) => {
                    if n != 0 {
                        // TASKS is unlocked before touching potentially lazy
                        // user memory. Pointers are trusted per this ABI.
                        unsafe { core::ptr::copy_nonoverlapping(buffer.as_ptr(), buf_ptr, n) };
                    }
                    n as u64
                }
                Err(_) => SYS_ERROR,
            }
        }
        SYS_CLOSE => {
            let fd = arg0 as usize;
            task::with_current_open_files(|table| match table.get_mut(fd) {
                Some(slot @ Some(_)) => {
                    *slot = None;
                    0
                }
                _ => SYS_ERROR,
            })
        }
        SYS_BRK => {
            let current = task::heap_end();
            let requested = arg0;
            if requested <= current {
                // A bare `brk(0)` (query the current break) and any
                // request to shrink both just report where the break
                // already is -- growing is the only thing this actually
                // implements, and doing nothing is a safe response to a
                // shrink request rather than an error.
                return current;
            }

            let target = requested.div_ceil(PAGE_SIZE) * PAGE_SIZE;
            let mut virt = current;
            while virt < target {
                let Some(phys) = pmm::alloc_frame() else {
                    break; // out of memory -- stop growing, hand back whatever was actually mapped
                };
                unsafe {
                    paging::map_page(virt, phys, paging::PAGE_WRITABLE | paging::PAGE_USER);
                    core::ptr::write_bytes((pmm::hhdm_offset() + phys) as *mut u8, 0, PAGE_SIZE as usize);
                }
                virt += PAGE_SIZE;
            }
            task::set_heap_end(virt);
            virt
        }
        SYS_CLONE => {
            let entry_virt = arg0;
            let stack_top = arg1;
            match task::spawn_thread("thread", entry_virt, stack_top) {
                Some(id) => id,
                None => SYS_ERROR, // out of task slots
            }
        }
        SYS_MMAP => {
            // Native ABI historically supplies only length, with RWX memory.
            crate::vm::map(0, arg0, 7, 0x22, None, 0)
                .map(|addr| addr as u64).unwrap_or(SYS_ERROR)
        }
        SYS_EXIT => {
            // Never returns -- task_exit() switches to a different task
            // entirely rather than coming back through this call.
            crate::task::task_exit();
        }
        _ => {
            println!("syscall: unknown number {number}");
            SYS_ERROR
        }
    }
}

// Entered via `int 0x80` from ring 3, so (unlike idt.rs's exception stubs,
// which sometimes fire at CPL0 with no stack switch) the CPU always pushes
// the full 5-word frame here: SS, RSP, RFLAGS, CS, RIP, in that order --
// there's a genuine privilege change (3 -> 0) every single time. Otherwise
// the same shape as timer.rs's isr_stub_32: full GPR save, fxsave/fxrstor
// via the *current* task's own buffer (see task.rs's module docs for why
// that has to be indirected through CURRENT_FXSAVE_PTR rather than a fixed
// buffer), except the call here is a genuine function call with arguments,
// not a bare `call` with no ABI contract.
global_asm!(
    r#"
.section .text

.global syscall_stub
syscall_stub:
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

    # Shuffle the caller's syscall convention (rax=number, rdi=arg0,
    # rsi=arg1, rdx=arg2, r10=arg3) into the SysV function-call convention
    # syscall_handler expects (rdi=number, rsi=arg0, rdx=arg1, rcx=arg2,
    # r8=arg3). This has to happen *before* the fxsave scratch register use
    # just below, which also wants r10 -- reversed, arg3 would already be
    # clobbered by the time it's read. Each move reads its source before an
    # earlier move overwrites it, working through the dependency chain from
    # the end backwards.
    mov rcx, rdx
    mov rdx, rsi
    mov rsi, rdi
    mov r8, r10
    mov rdi, rax

    mov r10, [rip + CURRENT_FXSAVE_PTR]
    fxsave [r10]

    call syscall_handler
    mov [rsp + 14*8], rax    # overwrite the saved rax slot with the return value

    mov r10, [rip + CURRENT_FXSAVE_PTR]
    fxrstor [r10]

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
