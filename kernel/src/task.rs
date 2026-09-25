//! Preemptive multitasking: a fixed-size table of tasks, a hand-written
//! assembly context switch, and a round-robin scheduler driven by
//! timer.rs's IRQ0 handler. Kernel threads (spawned via [`spawn`]) are
//! still just green threads sharing the one kernel address space -- no
//! isolation between them, nor would it mean much, since they're all
//! trusted kernel code. Ring-3 tasks (spawned via [`spawn_user`]) are
//! different: each one gets its own private address space (see
//! `paging::new_address_space`), so "process" is now the accurate word for
//! those specifically -- one user task's memory genuinely isn't reachable
//! from another's, not just conventionally off-limits.
//!
//! ## How the context switch actually works
//!
//! [`switch_to`] (the naked assembly routine below) only saves/restores the
//! System V ABI's callee-saved registers (rbp, rbx, r12-r15) plus RSP --
//! everything else is an ordinary function call boundary, so the compiler
//! already guarantees caller-saved registers don't need to survive it. That
//! makes it safe to call `switch_to` from plain Rust as long as every task
//! switch happens through the exact same call site: `irq0_handler`
//! (timer.rs) calls [`schedule`], which calls `switch_to`. A suspended
//! task's saved RSP always points at "partway through a `switch_to` call
//! inside `schedule` inside `irq0_handler` inside `isr_stub_32`" -- so
//! resuming it later just unwinds back up through that exact same call
//! chain, all the way out through `isr_stub_32`'s normal GPR-restore-and-
//! `iretq` epilogue, which correctly restores *that* task's own registers
//! and resumes *its* interrupted instruction, wherever that was.
//!
//! The one thing that isn't safe to share across tasks is the FXSAVE/
//! FXRSTOR area `isr_stub_32` uses to protect SSE state across the
//! interrupt: with a single fixed buffer (the pre-multitasking design),
//! saving task A's FPU state and then switching to task B before A ever
//! resumes means B's own timer interrupt overwrites A's saved state before
//! A gets it back. So every task owns its own 512-byte, 16-byte-aligned
//! FXSAVE area, and [`CURRENT_FXSAVE_PTR`] always points at whichever task
//! is *about to* run -- `schedule` updates it immediately before each
//! `switch_to`, and `isr_stub_32` re-reads it (never a cached value) after
//! `call irq0_handler` returns, so the `fxrstor` on the way out always
//! targets the buffer belonging to whichever task actually resumes there.
//!
//! A brand new task has never been through that call chain, so its stack is
//! hand-crafted (see [`spawn`]) to *look like* it has: the callee-saved
//! register slots `switch_to` expects to pop are prefilled (r12 holds the
//! task's entry-point function pointer -- a free way to hand it off without
//! any extra shared state), and the "return address" slot points at
//! [`task_trampoline`], which calls it.

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::paging;
use crate::sync::IrqSpinLock;

pub const MAX_TASKS: usize = 16;
const STACK_SIZE: usize = 32 * 1024;

/// Where a ring-3 task's heap starts, for `syscall.rs`'s `SYS_BRK` --
/// distinct from `loader.rs`'s `FLAT_BASE` and `USER_STACK_BASE`, in the
/// same low-canonical-address neighborhood. Every task gets this exact
/// same virtual address for the same reason `usermode.rs`'s and
/// `loader.rs`'s fixed addresses are safe to reuse across unrelated tasks:
/// each one lives in its own private address space, so there's no
/// collision.
pub const USER_HEAP_BASE: u64 = 0x0000_0060_0000_0000;

/// Where a ring-3 task's anonymous `SYS_MMAP` region starts -- its own
/// carved-out slice of address space, distinct from `USER_HEAP_BASE`
/// (`brk`, eagerly mapped) and `loader.rs`'s stack/code addresses. Unlike
/// `brk`, memory reserved here is never actually mapped at `mmap` time --
/// see `vm` and `paging::handle_page_fault` -- only backed
/// with a real physical frame the first time something actually touches
/// it.
pub const USER_MMAP_BASE: u64 = 0x0000_0070_0000_0000;

/// Where `isr_stub_32` (timer.rs) fxsaves/fxrstors on every timer
/// interrupt -- always the buffer belonging to whichever task is currently
/// meant to be running. `#[unsafe(no_mangle)]` purely so the raw assembly
/// in timer.rs can find it by a stable linker name; nothing outside
/// `init`/`schedule` in this module should ever touch it.
#[unsafe(no_mangle)]
pub static mut CURRENT_FXSAVE_PTR: u64 = 0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Ready,
    Running,
    /// Voluntarily off the run queue, waiting on a `futex()` word to change
    /// (see [`futex_wait`]/[`futex_wake`] and `linux_syscall.rs::sys_futex`)
    /// -- unlike `Terminated`, this is temporary: `futex_wake` flips it back
    /// to `Ready` once some other task satisfies whatever it's waiting for.
    /// Excluded from `schedule`'s selection the same way `Terminated` is
    /// (neither `Ready` nor "`Running` and it's the caller"), so a blocked
    /// task genuinely never gets a timeslice again until woken.
    Blocked,
    Terminated,
}

impl TaskState {
    pub fn label(self) -> &'static str {
        match self {
            TaskState::Ready => "ready",
            TaskState::Running => "running",
            TaskState::Blocked => "blocked",
            TaskState::Terminated => "terminated",
        }
    }
}

/// 512 bytes, 16-byte aligned -- what `fxsave`/`fxrstor` require of their
/// memory operand. The heap allocator only guarantees `heap::MIN_ALIGN`
/// (bumped to 16 specifically for this), which is what makes `Box::new`
/// here actually come back aligned instead of getting rejected.
#[repr(align(16))]
struct FxArea(#[allow(dead_code)] [u8; 512]); // Contents only ever touched by fxsave/fxrstor in asm, never Rust.

const _: () = assert!(core::mem::align_of::<FxArea>() == 16);
const _: () = assert!(core::mem::size_of::<FxArea>() == 512);

struct Task {
    id: u64,
    name: &'static str,
    state: TaskState,
    /// The *entire* saved context for a suspended task -- everything else
    /// (saved registers, the eventual resume point) lives on the stack this
    /// points into. See the module docs for why that's enough.
    saved_rsp: u64,
    /// Kept alive for as long as the task exists; never resized after
    /// `spawn`. `None` for task 0 (the boot thread), which is still running
    /// on whatever stack `_entry`/Limine originally set up rather than a
    /// heap-allocated one.
    _stack: Option<Box<[u8]>>,
    fxsave: Box<FxArea>,
    /// Top of this task's own stack -- what `gdt::set_kernel_stack` (TSS
    /// RSP0) gets pointed at right before this task runs, so that if it's
    /// ever running at CPL=3 and takes a syscall or an interrupt, the
    /// ring3->ring0 transition lands on a real, valid stack instead of
    /// whatever RSP0 last held. `0` for task 0 (no separately-tracked stack
    /// -- and it never runs below ring 0 in the first place, so RSP0 being
    /// stale for it is never actually consulted).
    kernel_stack_top: u64,
    /// How many timer ticks this task has actually been the one running --
    /// not a measure of useful work, just visible proof (via `ps`) that
    /// preemption is really round-robining across tasks.
    ticks_run: u64,
    /// Physical address of this task's PML4 -- what CR3 gets loaded to
    /// whenever the scheduler switches to this task. Every kernel thread
    /// shares the same value (`KERNEL_CR3`, captured once in `init()`); a
    /// ring-3 task built by `spawn_user` carries its own private one from
    /// `paging::new_address_space`. See the module docs and
    /// `paging::new_address_space` for what actually makes that isolating.
    cr3: u64,
    /// This task's current `brk` -- the top of its `SYS_BRK`-grown heap
    /// (see `syscall.rs`). Meaningless for kernel threads (nothing ever
    /// calls `brk` from ring 0), but harmless to carry on every `Task`
    /// rather than making it `Option`-wrapped for the ring-3-only case;
    /// always starts at `USER_HEAP_BASE` and only ever grows.
    heap_end: u64,
    /// Independent for new processes/native tasks; retained by Linux thread
    /// clones with CLONE_FILES. Last-owner drop releases the table, even when
    /// the creator exited first. See [`with_current_open_files`].
    open_files: Arc<IrqSpinLock<[Option<OpenFile>; MAX_OPEN_FILES]>>,
    /// This task's `FS_BASE` -- the pointer real x86_64 Linux userspace
    /// (via `arch_prctl(ARCH_SET_FS, ...)`, see `linux_syscall.rs`) points
    /// at its own TLS block, so every `%fs:offset` access (which is
    /// pervasive in real compiled C code -- `errno`, stack-protector
    /// canaries, musl's per-thread state) reads the *right* task's TLS,
    /// not whichever task happened to set `FS_BASE` last. `0` until
    /// something actually calls `arch_prctl` -- harmless, since nothing
    /// here dereferences `%fs` on its own behalf. Loaded into the real
    /// `FS_BASE` MSR by `schedule` on every switch, the same "only ever
    /// consulted through a context switch, never read back out of the
    /// live register" pattern `cr3`/`kernel_stack_top` already use.
    fs_base: u64,
    /// The address this task is currently blocked on via `FUTEX_WAIT` (see
    /// [`futex_wait`]), or `None` if it isn't blocked. Only meaningful while
    /// `state == TaskState::Blocked` -- [`futex_wake`] matches on this to
    /// decide which blocked tasks a given `FUTEX_WAKE` actually wakes.
    futex_addr: Option<u64>,
    // Absolute PIT tick deadline; only meaningful while Blocked.
    wait_deadline: Option<u64>,
    futex_timed_out: bool,
    /// Real Linux `CLONE_CHILD_CLEARTID`: the address `linux_syscall.rs`'s
    /// `sys_clone` was asked to zero (and futex-wake) the moment *this*
    /// task exits -- see [`task_exit`]. This is what makes real musl
    /// `pthread_join` work: it `FUTEX_WAIT`s on exactly this word being
    /// nonzero-then-zero. `None` for anything not spawned via
    /// `spawn_clone_raw` with that flag set (every kernel thread, every
    /// plain ring-3 process, and any clone that didn't ask for it).
    child_tidptr: Option<u64>,
    /// Real Linux `rt_sigaction`-installed signal handlers -- index by
    /// signal number (1-31; index 0 unused). `(handler, flags, restorer)`,
    /// all zero (`SIG_DFL`, no restorer) until a real `sigaction`/`signal`
    /// call sets one. Per-task, same privacy model as `open_files`: one
    /// task's handlers say nothing about another's. See
    /// [`SavedContext`]/[`begin_signal_delivery`] and item 25's README
    /// entry for what actually delivers one.
    sig_handlers: [(u64, u64, u64); NSIG],
    /// `Some(signum)` for exactly as long as this task is currently
    /// running inside a real, kernel-delivered signal handler -- set by
    /// [`begin_signal_delivery`], cleared by [`end_signal_delivery`]
    /// (`rt_sigreturn`). What `paging::handle_page_fault` checks before
    /// delivering a second `SIGSEGV` into a task already handling one:
    /// real Linux's default (no `SA_NODEFER`) behavior is to block the
    /// same signal for the duration of its own handler, so a fault that
    /// recurs inside the handler itself can't be delivered again and
    /// falls through to the ordinary fatal path instead -- the same "no
    /// infinite handler-faulting-into-itself loop" safety net real Linux
    /// has.
    sig_delivering: Option<u8>,
    /// The full pre-signal CPU context [`begin_signal_delivery`] snapshot,
    /// restorable by a real `rt_sigreturn` -- see [`SavedContext`].
    sig_saved_ctx: Option<SavedContext>,
    /// This task's real `argv[0]`/invocation path (see `loader.rs`'s
    /// `build_initial_stack` doc comment for why that's a genuinely
    /// different string from `name` above) -- kept here, not just written
    /// onto the initial stack and forgotten, because a real program can
    /// ask the *kernel* for it too: `readlink("/proc/self/exe", ...)`,
    /// which real glibc `ld.so` actually calls to resolve a `$ORIGIN` in
    /// its own `RPATH` (see `linux_syscall.rs`'s `sys_readlink`). Real
    /// Linux backs `/proc/self/exe` with the kernel's own record of which
    /// file `execve` actually ran, not a real symlink on disk; this is the
    /// same idea, just a `String` instead of a full procfs.
    exe_path: String,
}

/// A full, resumable ring-3 CPU context, captured by
/// `paging::handle_page_fault` at the moment a real `SIGSEGV` is about to
/// be delivered (every GPR, `RIP`/`CS`/`RFLAGS`/`RSP`/`SS`) and restored,
/// bit for bit, by [`sys_rt_sigreturn`] in `linux_syscall.rs` once the
/// handler calls its restorer trampoline. Deliberately *not* shaped to
/// match any of this kernel's existing stack-frame layouts (the `#PF`
/// GPR-push order, `linux_syscall_entry`'s 16-word frame, ...) -- it's a
/// standalone, named struct instead, because unlike those, nothing pushes
/// or pops it as raw stack words; `linux_syscall.rs`'s hand-written
/// `sigreturn_restore` asm reads each field by its own fixed offset (see
/// that function's doc comment and its `offset_of!` tripwires) and builds
/// a brand new `iretq` frame from scratch, restoring every register the
/// real x86_64 signal ABI is expected to -- including `RCX`/`R11`, which
/// an ordinary `sysretq`-based syscall return can *not* preserve (the
/// `syscall` instruction itself repurposes both), the reason this doesn't
/// just reuse `linux_syscall_resume_frame`'s existing resume path.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SavedContext {
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

/// Real Linux signal numbers only go up to 31 for the "standard" set (the
/// only ones this kernel delivers or even lets a task install a handler
/// for so far -- see the module docs). Index 0 is never used (there's no
/// signal 0 to install a handler for; real `kill(pid, 0)` is a liveness
/// probe with no handler semantics, not something this kernel implements
/// yet either).
const NSIG: usize = 32;

/// Small backing identity, independent of an open descriptor's position.
/// Static content supports the existing synthetic proc files without allocation.
#[derive(Clone, Copy)]
pub enum FileBacking {
    Disk(crate::fat16::File),
    Static(&'static [u8]),
}

impl FileBacking {
    pub fn len(&self) -> usize {
        match self { Self::Disk(file) => file.size as usize, Self::Static(data) => data.len() }
    }

    pub fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<usize, i64> {
        match self {
            Self::Disk(file) => file.read_at(offset, buffer).map_err(|_| -5), // EIO
            Self::Static(data) => {
                let n = (data.len() as u64).saturating_sub(offset).min(buffer.len() as u64) as usize;
                if n != 0 { buffer[..n].copy_from_slice(&data[offset as usize..offset as usize+n]); }
                Ok(n)
            }
        }
    }
}

pub struct OpenFile {
    pub data: FileBacking,
    pub pos: usize,
    /// A fake, but stable-per-path, inode number -- what `linux_syscall.rs`'s
    /// `sys_fstat` reports as `st_ino` (paired with a constant nonzero
    /// `st_dev`, since this kernel has exactly one filesystem). Needed for
    /// a real reason, not just polish: real musl `dlopen` de-duplicates
    /// already-loaded shared objects by comparing `(st_dev, st_ino)`
    /// against every object it's already loaded (including the
    /// interpreter and the main executable) -- see [`hash_path`]'s doc
    /// comment for what happened when every file honestly reported
    /// `(0, 0)` instead.
    pub ino: u64,
    /// `None` for the ordinary case `data` (`FileBacking`) already fully
    /// covers: a real, read-only regular file or static content, mmap-
    /// compatible (see `vm.rs`'s own `Region::backing`, which needs
    /// `FileBacking` to stay cheap and `Copy` -- exactly why this lives
    /// in a separate field instead of growing `FileBacking` itself with
    /// heap-allocated variants). `Some` for the two real capabilities
    /// added for `java -version`'s own `hsperfdata` PerfData file (see
    /// `docs/java-version.md`): an open directory descriptor (real
    /// `openat(..., O_DIRECTORY)`/`fchdir` needs its resolved path, not
    /// file content) and a real writable regular file, materialized
    /// fully in memory and flushed to `fat16` on close -- this
    /// filesystem's write path (`fat16::write_file`) is already a
    /// whole-buffer operation, not a real incremental one, so this
    /// doesn't lose anything a real per-offset disk write would have
    /// given a caller here. `data` is left as an inert `FileBacking::
    /// Static(&[])` placeholder whenever this is `Some`.
    pub extra: Option<Box<OpenExtra>>,
}

/// See [`OpenFile::extra`]'s own doc comment for why this exists as a
/// separate type instead of two more `FileBacking` variants.
pub enum OpenExtra {
    /// A real, already-resolved absolute path to an open directory --
    /// `linux_syscall.rs`'s `sys_fchdir` reads it back to call
    /// `fat16::change_dir`, the same real Linux dance a real glibc/
    /// HotSpot uses (`openat(dir, O_DIRECTORY)` + `fchdir`) to create a
    /// file by a bare relative name inside a specific directory without
    /// building a full path string itself.
    Dir(String),
    /// A real regular file's full content, plus the real path it'll be
    /// flushed back to on close. `write`/`pwrite`/`ftruncate` all operate
    /// directly on this `Vec` (see `linux_syscall.rs`'s own doc comments
    /// on those); nothing here is a demand-paged/lazy read the way a
    /// `FileBacking::Disk` read is, because a small, actively-written
    /// file (this exists for a 32 KiB PerfData region, not a multi-
    /// megabyte archive) has no real reason to be.
    Writable(String, Vec<u8>),
}

/// A simple FNV-1a hash of a file's path, used as [`OpenFile::ino`] --
/// stable for the same path across repeated opens (real inode semantics:
/// the same file opened twice should compare equal), and, in practice,
/// distinct enough between different paths that a real `dlopen` never
/// confuses two different files as being the same one.
///
/// This exists because of a real bug, not speculative caution: the first
/// version of `linux_syscall.rs`'s `sys_fstat` reported `st_ino: 0` for
/// every file, same as `st_dev`. Real musl `dlopen` uses exactly that
/// pair to skip re-loading a shared object it's already mapped -- and
/// since musl's own already-loaded `ldso`/`app` bookkeeping entries also
/// default to `(0, 0)` until something sets them otherwise, a `dlopen`'d
/// library that honestly reported `(0, 0)` too was *indistinguishable*
/// from those already-loaded entries: musl silently treated a brand new
/// `dlopen("./dlfoo.so")` as "oh, that's just the interpreter again,
/// already loaded" and handed back a handle to `libc.so` instead --
/// `dlopen` itself reported success, but `dlsym` correctly failed to find
/// a symbol that (unsurprisingly) doesn't exist in `libc.so`. Caught via
/// that exact symptom in QEMU, not by inspection -- see item 24's README
/// entry.
pub fn hash_path(path: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in path.as_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    // A hash of 0 would silently recreate the exact bug this exists to
    // fix (an empty path hashes to the FNV offset basis, which is
    // nonzero, so this is only a theoretical guard, not a real case that
    // occurs -- kept for the same "never silently reintroduce (0, 0)"
    // reason the rest of this function exists).
    if hash == 0 { 1 } else { hash }
}

pub const MAX_OPEN_FILES: usize = 16;

static TASKS: IrqSpinLock<[Option<Task>; MAX_TASKS]> = IrqSpinLock::new([const { None }; MAX_TASKS]);
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
/// The address space every kernel thread runs in, captured once in
/// [`init`] (at that point it's still whatever Limine + `heap::init` +
/// friends left CR3 pointing at). `spawn` hands this to every kernel
/// thread it creates; it's never switched away from except transiently,
/// while a ring-3 task is running.
static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);

// enter_user_mode hardcodes these selector values directly (see its asm
// below) rather than relying on cross-language const substitution into
// global_asm! -- this is the tripwire that catches gdt.rs's layout ever
// changing out from under it.
const _: () = assert!(crate::gdt::USER_CODE_SELECTOR == 0x2B, "update enter_user_mode's hardcoded selector");
const _: () = assert!(crate::gdt::USER_DATA_SELECTOR == 0x33, "update enter_user_mode's hardcoded selector");

unsafe extern "C" {
    fn switch_to(current_rsp: *mut u64, next_rsp: u64);
    fn task_trampoline();
    fn enter_user_mode();
}

/// Writes `value` at the next lower 8-byte-aligned slot below `*sp`,
/// updating `*sp` to point at it -- the moral equivalent of a `push`, used
/// by [`spawn`]/[`spawn_user`] to hand-build a task's initial stack before
/// it's ever run.
unsafe fn push(sp: &mut u64, value: u64) {
    *sp -= 8;
    unsafe {
        (*sp as *mut u64).write(value);
    }
}

core::arch::global_asm!(
    r#"
.section .text

.global switch_to
switch_to:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

.global task_trampoline
task_trampoline:
    # A brand new task lands here via switch_to's `ret`, never an `iretq`,
    # so unlike every *other* resume point (which restores IF along with
    # the rest of the flags register a real interrupt frame carries) there
    # is nothing that would otherwise re-enable interrupts -- without this
    # `sti`, a freshly spawned task would run with IF permanently clear
    # from having been entered mid-timer-interrupt, and never get preempted
    # again (confirmed the hard way: it hangs the whole machine solid,
    # since nothing else ever gets the CPU back).
    sti
    # r12 holds this task's entry-point function pointer, stashed there by
    # `spawn`'s hand-crafted initial stack and landed in the real r12
    # register by switch_to's own `pop r12` just before the `ret` that
    # brought us here.
    call r12
    call task_exit
    # task_exit is `-> !`; this is an unreachable safety net in case that
    # ever changes.
    cli
1:  hlt
    jmp 1b

.global enter_user_mode
enter_user_mode:
    # Reached the same way task_trampoline is (switch_to's `ret`, never an
    # `iretq`), with `spawn_user`'s stack-crafting stashing the user-mode
    # entry point in r12 and the user stack pointer in r13 -- but unlike a
    # kernel thread, this task's *first* instruction has to actually run at
    # CPL=3, and a privilege change can only happen through `iretq` (or
    # `sysret`, unused here), never a plain `call`/`jmp`. So instead of
    # calling into r12 directly, this builds the same 5-word frame a real
    # ring3->ring0->ring3 round trip would have left on the stack, then
    # `iretq`s through it once, purely to get the ball rolling.
    #
    # 0x33/0x2B are USER_DATA_SELECTOR/USER_CODE_SELECTOR (gdt.rs) with
    # RPL=3 already folded in -- see the const asserts above pinning them.
    mov ax, 0x33
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax

    push 0x33          # SS (user data, RPL=3)
    push r13           # RSP (user stack top, from spawn_user)
    pushfq             # RFLAGS -- whatever this task's were left at
    or qword ptr [rsp], 0x200   # force IF=1: see task_trampoline's note above,
                                # same underlying problem (this never went
                                # through a real interrupt frame to inherit
                                # IF=1 from) but iretq restores from the
                                # stack instead of just letting `sti` run,
                                # so the fix has to live in the pushed value.
    push 0x2B          # CS (user code, RPL=3)
    push r12           # RIP (user entry point, from spawn_user)
    iretq
"#
);

/// # Safety
/// Must be called exactly once, after the heap is up and before `sti`.
/// Registers task 0 as a stand-in for whoever's calling this (the boot
/// thread that's about to run the shell) so `schedule()` has a "current
/// task" to switch away from and eventually back to.
pub unsafe fn init() {
    KERNEL_CR3.store(paging::current_cr3(), Ordering::Relaxed);

    let fxsave = Box::new(FxArea([0; 512]));
    unsafe {
        CURRENT_FXSAVE_PTR = (&*fxsave) as *const FxArea as u64;
    }

    let mut tasks = TASKS.lock();
    tasks[0] = Some(Task {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed) as u64,
        name: "shell",
        state: TaskState::Running,
        saved_rsp: 0, // Not read until this task is first switched away from.
        _stack: None,
        fxsave,
        kernel_stack_top: 0, // Never consulted -- see the field's own doc comment.
        ticks_run: 0,
        cr3: KERNEL_CR3.load(Ordering::Relaxed),
        heap_end: USER_HEAP_BASE,
        open_files: Arc::new(IrqSpinLock::new([const { None }; MAX_OPEN_FILES])),
        fs_base: 0,
        futex_addr: None,
        wait_deadline: None,
        futex_timed_out: false,
        child_tidptr: None,
        sig_handlers: [(0, 0, 0); NSIG],
        sig_delivering: None,
        sig_saved_ctx: None,
        exe_path: String::new(),
    });
    CURRENT.store(0, Ordering::Relaxed);
}

/// Spawns a new kernel thread running `entry`, with the default
/// `STACK_SIZE` (32 KiB) -- plenty for the shell and small demo threads
/// this kernel has run so far. If `entry` ever returns, the task is marked
/// terminated and never scheduled again (see [`task_exit`]). Returns the
/// new task's ID, or `None` if the task table is full.
pub fn spawn(name: &'static str, entry: fn()) -> Option<u64> {
    spawn_with_stack(name, entry, STACK_SIZE)
}

/// Same as [`spawn`], but with an explicit stack size instead of the
/// default -- for a task that's going to need real headroom, like a future
/// DOOM task (a whole C game engine's call depth, versus this kernel's own
/// shallow, hand-written Rust). Rounded up to a 16-byte-aligned size same
/// as the default path; there's no enforced ceiling, just whatever the
/// heap can actually back.
pub fn spawn_with_stack(name: &'static str, entry: fn(), stack_size: usize) -> Option<u64> {
    let stack_size = (stack_size + 15) & !15;
    let stack: Box<[u8]> = vec![0u8; stack_size].into_boxed_slice();
    let top = stack.as_ptr() as u64 + stack_size as u64;
    debug_assert!(top % 16 == 0, "task stack must end 16-byte aligned");

    // Build the frame switch_to's epilogue expects to unwind into: a
    // return address of task_trampoline, then the six callee-saved slots it
    // pops (pop order: r15, r14, r13, r12, rbx, rbp) -- with r12 repurposed
    // to carry `entry` through to the trampoline instead of a real saved
    // register, since this task has never actually run yet.
    let mut sp = top;
    unsafe {
        push(&mut sp, task_trampoline as *const () as u64); // return address
        push(&mut sp, 0); // rbp
        push(&mut sp, 0); // rbx
        push(&mut sp, entry as *const () as u64); // r12 <- entry fn pointer
        push(&mut sp, 0); // r13
        push(&mut sp, 0); // r14
        push(&mut sp, 0); // r15
    }

    let mut tasks = TASKS.lock();
    let slot = tasks.iter().position(|t| t.is_none())?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) as u64;
    tasks[slot] = Some(Task {
        id,
        name,
        state: TaskState::Ready,
        saved_rsp: sp,
        _stack: Some(stack),
        fxsave: Box::new(FxArea([0; 512])),
        kernel_stack_top: top,
        ticks_run: 0,
        cr3: KERNEL_CR3.load(Ordering::Relaxed),
        heap_end: USER_HEAP_BASE,
        open_files: Arc::new(IrqSpinLock::new([const { None }; MAX_OPEN_FILES])),
        fs_base: 0,
        futex_addr: None,
        wait_deadline: None,
        futex_timed_out: false,
        child_tidptr: None,
        sig_handlers: [(0, 0, 0); NSIG],
        sig_delivering: None,
        sig_saved_ctx: None,
        exe_path: String::new(),
    });
    Some(id)
}

/// Spawns a new **ring-3** task: `entry_virt`/`user_stack_top` are virtual
/// addresses the caller has already mapped `PAGE_USER` *within `cr3`*
/// (see `usermode.rs`, the only current caller, which builds `cr3` via
/// `paging::new_address_space` before calling this) -- this just arranges
/// for the very first resume to drop into them via [`enter_user_mode`]
/// instead of calling straight into `entry` the way a kernel thread does,
/// since a privilege change can only happen through `iretq`. The task
/// still gets its own ordinary (ring-0-only) stack too, exactly like
/// [`spawn`]'s -- that one becomes this task's TSS RSP0, i.e. where a
/// syscall or interrupt from its ring-3 code lands, kept entirely separate
/// from the ring-3 stack at `user_stack_top`. Unlike a kernel thread's
/// stack, this one lives in the *shared kernel half* (an ordinary heap
/// allocation), not `cr3`'s private low half -- it's only ever touched
/// from ring 0, so it doesn't need to be, and every address space can see
/// it regardless of which one happens to be active when it's used.
pub fn spawn_user(name: &'static str, exe_path: String, entry_virt: u64, user_stack_top: u64, cr3: u64) -> Option<u64> {
    let stack: Box<[u8]> = vec![0u8; STACK_SIZE].into_boxed_slice();
    let top = stack.as_ptr() as u64 + STACK_SIZE as u64;
    debug_assert!(top % 16 == 0, "task stack must end 16-byte aligned");

    let mut sp = top;
    unsafe {
        push(&mut sp, enter_user_mode as *const () as u64); // return address
        push(&mut sp, 0); // rbp
        push(&mut sp, 0); // rbx
        push(&mut sp, entry_virt); // r12 <- user-mode entry point (RIP)
        push(&mut sp, user_stack_top); // r13 <- user-mode stack pointer (RSP)
        push(&mut sp, 0); // r14
        push(&mut sp, 0); // r15
    }

    let mut tasks = TASKS.lock();
    let slot = tasks.iter().position(|t| t.is_none())?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) as u64;
    tasks[slot] = Some(Task {
        id,
        name,
        state: TaskState::Ready,
        saved_rsp: sp,
        _stack: Some(stack),
        fxsave: Box::new(FxArea([0; 512])),
        kernel_stack_top: top,
        ticks_run: 0,
        cr3,
        heap_end: USER_HEAP_BASE,
        open_files: Arc::new(IrqSpinLock::new([const { None }; MAX_OPEN_FILES])),
        fs_base: 0,
        futex_addr: None,
        wait_deadline: None,
        futex_timed_out: false,
        child_tidptr: None,
        sig_handlers: [(0, 0, 0); NSIG],
        sig_delivering: None,
        sig_saved_ctx: None,
        exe_path,
    });
    Some(id)
}

/// Spawns a new ring-3 task that shares the *calling* task's address space
/// instead of getting a fresh private one from `paging::new_address_space`
/// -- a thread, not a process: same code, same data, same mappings,
/// different stack and registers. What `syscall.rs`'s `SYS_CLONE` uses to
/// give a ring-3 program real concurrency within one address space, the
/// way pthreads (and, down the road, a JVM's GC/JIT/render threads) expect
/// -- see the module docs' "process vs thread" distinction, which this is
/// the second half of.
///
/// `entry_virt`/`stack_top` are trusted to already be valid, mapped
/// `PAGE_USER` addresses within the caller's own address space -- same
/// trust model every other syscall argument in `syscall.rs` uses (see its
/// module docs): this function doesn't map anything itself, unlike
/// `spawn_user`'s callers (`loader.rs`/`usermode.rs`), which build a brand
/// new, empty address space and have to populate it from nothing. A
/// caller here is expected to have already carved out a stack region for
/// the new thread (e.g. via `SYS_BRK`) before asking for it to run.
///
/// mmap metadata is shared by CR3 in vm.rs. The program break remains a
/// per-thread snapshot, and this native clone's descriptors start empty.
/// Linux spawn_clone_raw instead retains the table for CLONE_FILES.
/// These are remaining limitations of the Linux thread model.
///
/// The address-space-teardown correctness this depends on lives in
/// `schedule`'s reaping sweep: since two tasks can now legitimately share
/// one `cr3`, it only actually frees that address space once *no* task
/// slot still points at it, not just whenever any one sharer terminates.
pub fn spawn_thread(name: &'static str, entry_virt: u64, stack_top: u64) -> Option<u64> {
    let stack: Box<[u8]> = vec![0u8; STACK_SIZE].into_boxed_slice();
    let top = stack.as_ptr() as u64 + STACK_SIZE as u64;
    debug_assert!(top % 16 == 0, "task stack must end 16-byte aligned");

    let mut sp = top;
    unsafe {
        push(&mut sp, enter_user_mode as *const () as u64); // return address
        push(&mut sp, 0); // rbp
        push(&mut sp, 0); // rbx
        push(&mut sp, entry_virt); // r12 <- user-mode entry point (RIP)
        push(&mut sp, stack_top); // r13 <- user-mode stack pointer (RSP)
        push(&mut sp, 0); // r14
        push(&mut sp, 0); // r15
    }

    let mut tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    // Same honest snapshot-not-shared caveat as heap_end (see this
    // function's doc comment): a sibling thread that later calls
    let (cr3, heap_end, fs_base, exe_path) = {
        let parent = tasks[current].as_ref()?;
        (parent.cr3, parent.heap_end, parent.fs_base, parent.exe_path.clone())
    };
    let slot = tasks.iter().position(|t| t.is_none())?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) as u64;
    tasks[slot] = Some(Task {
        id,
        name,
        state: TaskState::Ready,
        saved_rsp: sp,
        _stack: Some(stack),
        fxsave: Box::new(FxArea([0; 512])),
        kernel_stack_top: top,
        ticks_run: 0,
        cr3,
        heap_end,
        open_files: Arc::new(IrqSpinLock::new([const { None }; MAX_OPEN_FILES])),
        fs_base,
        futex_addr: None,
        wait_deadline: None,
        futex_timed_out: false,
        child_tidptr: None,
        sig_handlers: [(0, 0, 0); NSIG],
        sig_delivering: None,
        sig_saved_ctx: None,
        exe_path,
    });
    Some(id)
}

/// Spawns a **real Linux `clone()`** thread -- the second half of
/// `linux_syscall.rs`'s `sys_clone`, which is `spawn_thread`'s real-ABI
/// sibling: a task that shares its caller's address space, but has to
/// *resume as if it had made the exact same `syscall` instruction the
/// parent did*, not at some fresh, kernel-chosen entry point. Real glibc/
/// musl `clone()` relies on this: the child comes back from `syscall` with
/// `rax=0` (vs. the parent's `rax=<child tid>`) on whatever stack
/// `child_stack` says, and picks up running its own hand-written
/// trampoline asm from there -- see `linux_syscall.rs`'s module docs for
/// the full "resuming mid-syscall-epilogue" architecture this implements.
///
/// `resume_addr` is the address of `linux_syscall.rs`'s
/// `linux_syscall_resume_frame` label -- a midpoint inside
/// `linux_syscall_entry`'s own assembly (the `fxrstor` + 16-pop + `sysretq`
/// epilogue every real syscall already exits through), reached here via
/// the exact same `switch_to`-compatible hand-built-stack `ret` mechanism
/// [`spawn`]/[`spawn_thread`] already use for `task_trampoline`/
/// `enter_user_mode` -- just pointed at a different label. This function
/// doesn't know or care what that label actually does; it's handed in by
/// `linux_syscall.rs` (which does) purely so this module doesn't have to
/// depend on it.
///
/// `frame` is the 16 u64s of a live `linux_syscall_entry` register frame,
/// in the exact ascending-address order that asm pushes/pops them (see
/// `linux_syscall.rs::sys_clone`'s doc comment for the index-by-index
/// layout) -- normally a raw copy of the *parent's* own in-flight frame,
/// with index 12 (rax) patched to 0 and index 15 (the saved user RSP)
/// patched to `child_stack`, both already done by the caller before this
/// is reached. This function just writes those 16 words onto the new
/// task's own kernel stack, immediately below the hand-built `switch_to`
/// frame, so that when `switch_to`'s `ret` lands on `resume_addr`, RSP is
/// already sitting exactly where `linux_syscall_entry`'s own epilogue
/// expects its 16-word frame to be.
///
/// `child_tidptr` is `Some` exactly when the caller set
/// `CLONE_CHILD_CLEARTID` -- see [`Task::child_tidptr`]'s doc comment and
/// [`task_exit`] for what actually happens with it.
///
/// heap_end remains a snapshot as in spawn_thread. The syscall caller requires
/// CLONE_FILES; retain its table instead of creating an empty child table.
pub fn spawn_clone_raw(
    name: &'static str,
    resume_addr: u64,
    frame: [u64; 16],
    cr3: u64,
    heap_end: u64,
    fs_base: u64,
    child_tidptr: Option<u64>,
    exe_path: String,
) -> Option<u64> {
    let stack: Box<[u8]> = vec![0u8; STACK_SIZE].into_boxed_slice();
    let top = stack.as_ptr() as u64 + STACK_SIZE as u64;
    debug_assert!(top % 16 == 0, "task stack must end 16-byte aligned");

    // Write the 16-word linux_syscall_entry frame at [top-128, top),
    // ascending address order matching the asm's own push order exactly
    // (see linux_syscall.rs::sys_clone's doc comment) -- this is *not*
    // built via the `push` helper (which writes descending, like a real
    // `push` instruction) since the caller already assembled it in the
    // matching ascending layout as a plain array.
    let frame_base = top - (frame.len() as u64) * 8;
    unsafe {
        for (i, word) in frame.iter().enumerate() {
            ((frame_base + (i as u64) * 8) as *mut u64).write(*word);
        }
    }

    // Below that, the ordinary switch_to-compatible frame every other
    // spawn_* function here builds -- just pointing its "return address"
    // at resume_addr instead of task_trampoline/enter_user_mode.
    let mut sp = frame_base;
    unsafe {
        push(&mut sp, resume_addr); // return address
        push(&mut sp, 0); // rbp
        push(&mut sp, 0); // rbx
        push(&mut sp, 0); // r12
        push(&mut sp, 0); // r13
        push(&mut sp, 0); // r14
        push(&mut sp, 0); // r15
    }

    // Retain the parent table independently of either task slot lifetime.
    let open_files = current_open_files();
    let mut tasks = TASKS.lock();
    let slot = tasks.iter().position(|t| t.is_none())?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) as u64;
    tasks[slot] = Some(Task {
        id,
        name,
        state: TaskState::Ready,
        saved_rsp: sp,
        _stack: Some(stack),
        fxsave: Box::new(FxArea([0; 512])),
        kernel_stack_top: top,
        ticks_run: 0,
        cr3,
        heap_end,
        open_files,
        fs_base,
        futex_addr: None,
        wait_deadline: None,
        futex_timed_out: false,
        child_tidptr,
        sig_handlers: [(0, 0, 0); NSIG],
        sig_delivering: None,
        sig_saved_ctx: None,
        exe_path,
    });
    Some(id)
}

/// Marks the current task terminated and switches away for good. Called by
/// `task_trampoline` when a kernel thread's entry function returns
/// normally, by `syscall.rs`'s `SYS_EXIT` handler when a ring-3 task calls
/// `exit()`, and by `libc_shim.rs`'s `exit`/`abort` (via [`exit_current`])
/// when a kernel-thread C task like DOOM calls either. Never returns to its
/// caller: the task's stack and fxsave area stay allocated for one more
/// scheduling round (still technically in use by this very call frame right
/// up until the context switch below), then get freed and the slot reused
/// by `schedule`'s reaping sweep the next time it runs on someone else's
/// stack -- see its doc comment.
///
/// Also implements real Linux `CLONE_CHILD_CLEARTID`: if this task was
/// spawned via `spawn_clone_raw` with `child_tidptr` set (see
/// [`Task::child_tidptr`]), the real kernel's contract is to zero that
/// 32-bit word and `FUTEX_WAKE` anyone waiting on it, the instant this
/// task actually exits -- not before. This is what makes a real musl
/// `pthread_join` unblock: it `FUTEX_WAIT`s on exactly this address for
/// exactly this transition. The write happens here, before `schedule()`
/// switches away, while this task's own address space (where `child_tidptr`
/// lives) is still the one loaded into CR3 -- same trusted-pointer model
/// every other syscall in this codebase already uses.
#[unsafe(no_mangle)]
pub extern "C" fn task_exit() -> ! {
    let child_tidptr = {
        let mut tasks = TASKS.lock();
        let current = CURRENT.load(Ordering::Relaxed);
        if let Some(task) = &mut tasks[current] {
            task.state = TaskState::Terminated;
            task.child_tidptr.take()
        } else {
            None
        }
    };
    if let Some(ptr) = child_tidptr {
        unsafe {
            (ptr as *mut u32).write(0);
        }
        futex_wake(ptr, u32::MAX);
    }
    schedule();
    // schedule() only returns without switching when there's nowhere else
    // to go, which can't happen for a task that just marked itself
    // terminated (it's excluded from selection) -- but there's no valid
    // saved context left to resume here, so spin rather than run on.
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

/// Ergonomic alias for [`task_exit`] -- for Rust call sites (like
/// `libc_shim.rs`'s `exit`/`abort`) that want to terminate the current task
/// without going through `task_exit`'s `extern "C"` name, which exists
/// mainly so `task_trampoline`'s hand-written assembly can call it by a
/// stable symbol. Never returns.
pub fn exit_current() -> ! {
    task_exit()
}

/// Terminates *another* task by ID -- for example the shell's `kill`
/// command stopping a runaway or unwanted task (DOOM included) without
/// waiting for it to exit on its own. Unlike [`exit_current`], this doesn't
/// need to switch away from anything: it just flips the target's state to
/// `Terminated`, which permanently excludes it from `schedule`'s selection
/// from that point on. The target keeps running until the *next*
/// scheduling point (at most one timer tick away), at which point it's
/// switched out for good and its slot is reaped exactly as if it had called
/// `exit_current` on itself.
///
/// Returns `false` if no live (non-terminated) task has this ID -- either
/// it never existed, or it's already dead (mid-teardown or already reaped).
pub fn kill(id: u64) -> bool {
    let mut tasks = TASKS.lock();
    for slot in tasks.iter_mut() {
        if let Some(task) = slot {
            if task.id == id && task.state != TaskState::Terminated {
                task.state = TaskState::Terminated;
                return true;
            }
        }
    }
    false
}

/// Round-robin: picks the next `Ready` task after the current one (falling
/// back to the current task itself if it's still `Running` and nothing
/// else is `Ready`), and performs the actual context switch if that's
/// someone new. Called from `irq0_handler` on every timer tick; also
/// callable directly (see [`yield_now`]) for a task that wants to give up
/// its slice early.
pub fn schedule() {
    let (current_rsp_ptr, next_rsp, next_fxsave_ptr, next_kernel_stack_top, next_cr3, next_fs_base, same) = {
        let mut tasks = TASKS.lock();
        let current = CURRENT.load(Ordering::Relaxed);

        // Timer scheduling and explicit wakes serialize under TASKS on this
        // single CPU. Only Blocked tasks can transition, so the winner fixes
        // the return reason and clears the deadline before the task runs.
        let now = crate::timer::ticks();
        for task in tasks.iter_mut().flatten() {
            if task.state == TaskState::Blocked
                && task.wait_deadline.is_some_and(|deadline| now >= deadline) {
                task.state = TaskState::Ready;
                task.futex_addr = None;
                task.wait_deadline = None;
                task.futex_timed_out = true;
            }
        }

        // Reap any task that finished running before this call (marked
        // Terminated by task_exit/kill, either on a previous schedule() or
        // just now, below, before we ever got here). A Terminated task is
        // permanently excluded from selection, so once it's not the one we
        // arrived here still running on top of, nothing can still be using
        // its stack or fxsave area -- dropping the Task here frees both
        // (Task's `_stack`/`fxsave` Boxes) and clears the slot for reuse.
        // `i == current` is always skipped: if the *current* task just
        // terminated itself (task_exit), we're still executing on its
        // stack right up through this very function call, so freeing it
        // here would pull the rug out from under our own return address.
        // That one slot gets picked up on a later call instead -- there's
        // always one coming, at minimum the next 100Hz timer tick, running
        // on whatever task we're about to switch to below.
        for i in 0..MAX_TASKS {
            if i == current {
                continue;
            }
            if matches!(&tasks[i], Some(t) if t.state == TaskState::Terminated) {
                // A ring-3 task's `cr3` is a private PML4 (see
                // `spawn_user`/`paging::new_address_space`), distinct from
                // every kernel thread's shared `KERNEL_CR3`. Free its whole
                // private half -- code/stack/heap frames plus the
                // PDPT/PD/PT frames mapping them -- before dropping the
                // slot; a kernel-thread `Task` (whose `cr3` *is*
                // `KERNEL_CR3`) owns no private address space, so it's left
                // alone. Safe to do here: this task is guaranteed not to be
                // the one currently loaded into CR3 (see the comment above
                // this loop -- `i == current` is always skipped).
                //
                // Since `spawn_thread`, a `cr3` isn't necessarily private to
                // *this one task* anymore -- sibling threads share it. So
                // before freeing it, check whether any other still-present
                // slot (Ready/Running/already-Terminated-but-not-yet-reaped,
                // doesn't matter which) still points at the same `cr3`; if
                // so, some other task's memory still lives there and
                // freeing it now would pull the rug out from under a task
                // that hasn't even run its last instruction yet. Only the
                // last thread out actually tears it down -- a simple linear
                // scan under the same `TASKS` lock this whole loop already
                // holds, cheap at `MAX_TASKS` = 16 and always consistent
                // since nothing else can be mutating the table concurrently.
                if let Some(t) = &tasks[i] {
                    let cr3 = t.cr3;
                    let still_shared = cr3 != KERNEL_CR3.load(Ordering::Relaxed)
                        && tasks.iter().enumerate().any(|(j, other)| j != i && matches!(other, Some(o) if o.cr3 == cr3));
                    if cr3 != KERNEL_CR3.load(Ordering::Relaxed) && !still_shared {
                        unsafe { paging::destroy_address_space(cr3) };
                    }
                }
                tasks[i] = None;
            }
        }

        let mut next = current;
        for offset in 1..=MAX_TASKS {
            let candidate = (current + offset) % MAX_TASKS;
            if let Some(task) = &tasks[candidate] {
                let eligible = task.state == TaskState::Ready
                    || (candidate == current && task.state == TaskState::Running);
                if eligible {
                    next = candidate;
                    break;
                }
            }
        }

        if next == current {
            (core::ptr::null_mut(), 0u64, 0u64, 0u64, 0u64, 0u64, true)
        } else {
            if let Some(task) = &mut tasks[current] {
                if task.state == TaskState::Running {
                    task.state = TaskState::Ready;
                }
            }
            let current_rsp_ptr = &mut tasks[current].as_mut().unwrap().saved_rsp as *mut u64;
            let next_task = tasks[next].as_mut().unwrap();
            next_task.state = TaskState::Running;
            next_task.ticks_run += 1;
            let next_rsp = next_task.saved_rsp;
            let next_fxsave_ptr = (&*next_task.fxsave) as *const FxArea as u64;
            let next_kernel_stack_top = next_task.kernel_stack_top;
            let next_cr3 = next_task.cr3;
            let next_fs_base = next_task.fs_base;
            CURRENT.store(next, Ordering::Relaxed);
            (current_rsp_ptr, next_rsp, next_fxsave_ptr, next_kernel_stack_top, next_cr3, next_fs_base, false)
        }
    };

    if same {
        return;
    }

    if next_kernel_stack_top != 0 {
        // See gdt::set_kernel_stack and Task::kernel_stack_top -- this is
        // what makes a ring3->ring0 transition on the incoming task (a
        // syscall, today; potentially a fault later) land somewhere valid.
        unsafe {
            crate::gdt::set_kernel_stack(next_kernel_stack_top);
        }
    }

    // Only actually reload CR3 (and pay for the full TLB flush that comes
    // with it) when the incoming task's address space genuinely differs --
    // true for a ring3<->kernel-thread switch, but *not* for the far more
    // common kernel-thread<->kernel-thread switch (shell/counter-a/
    // counter-b all share KERNEL_CR3), which would otherwise eat a needless
    // flush on every single 100Hz tick.
    if next_cr3 != paging::current_cr3() {
        unsafe {
            paging::load_cr3(next_cr3);
        }
    }

    // FS_BASE, unlike CR3, is cheap to reload unconditionally (a plain
    // `wrmsr`, no TLB flush) -- what makes %fs:-relative TLS accesses
    // (errno, stack-protector canaries, musl's per-thread state -- see
    // linux_syscall.rs and Task::fs_base's doc comment) resolve against
    // *this* task's own TLS block the instant it starts running, not
    // whichever task last called arch_prctl.
    const IA32_FS_BASE: u32 = 0xC000_0100;
    unsafe {
        crate::gdt::wrmsr(IA32_FS_BASE, next_fs_base);
    }

    // Safety: `current_rsp_ptr` points into TASKS's fixed-size array, which
    // is a `static` and never moves or reallocates, so it stays valid past
    // the lock guard above being dropped. Nothing else can race it: this
    // whole function only ever runs with interrupts disabled (we're always
    // either inside the timer ISR, or in `yield_now` before any other task
    // has had a chance to run on this single-core kernel).
    unsafe {
        CURRENT_FXSAVE_PTR = next_fxsave_ptr;
        switch_to(current_rsp_ptr, next_rsp);
    }
}

/// Voluntarily gives up the rest of this task's timeslice. Not required --
/// preemption via the timer means every `Ready` task gets a turn regardless
/// -- but useful for a task that knows it has nothing to do right now.
#[allow(dead_code)]
pub fn yield_now() {
    schedule();
}

/// Real `FUTEX_WAIT`: marks the current task [`TaskState::Blocked`] on
/// `addr` and immediately gives up the CPU via `schedule()` -- what
/// `linux_syscall.rs::sys_futex` uses so a real musl mutex/condvar/
/// `pthread_join` actually sleeps instead of busy-spinning. Never returns
/// until its deadline expires or a later [`futex_wake`] call (including one
/// running on behalf of a completely different syscall -- that's the whole
/// point) flips this task back to `Ready`, at which point `schedule` picks
/// it back up like any other ready task. Returns true only on timeout.
///
/// The actual value comparison (`*addr == expected`) that real
/// `FUTEX_WAIT` does atomically before blocking -- to avoid the classic
/// lost-wakeup race where the waker runs between the caller's own check
/// and the block -- is the caller's job (`sys_futex`), not this function's;
/// this only implements the "go to sleep, tagged with this address"
/// half. Safe enough here specifically because this whole kernel is
/// single-core and every syscall runs with interrupts disabled for its
/// entire duration (see `linux_syscall.rs`'s module docs) -- nothing else
/// can run *at all* between `sys_futex`'s read of `*addr` and this call,
/// so there's no window for the race to actually occur.
pub fn futex_wait(addr: u64, deadline: Option<u64>) -> bool {
    block_until(Some(addr), deadline)
}

/// A timed sleep has no futex address, so FUTEX_WAKE cannot end it early.
/// Like futex_wait, called from an IRQ-disabled syscall on the single CPU.
pub fn sleep_until(deadline: u64) {
    let _ = block_until(None, Some(deadline));
}

fn block_until(addr: Option<u64>, deadline: Option<u64>) -> bool {
    if deadline.is_some_and(|end| crate::timer::ticks() >= end) {
        return true;
    }
    {
        let mut tasks = TASKS.lock();
        let current = CURRENT.load(Ordering::Relaxed);
        if let Some(task) = &mut tasks[current] {
            task.state = TaskState::Blocked;
            task.futex_addr = addr;
            task.wait_deadline = deadline;
            task.futex_timed_out = false;
        }
    }
    schedule();
    let mut tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    if let Some(task) = &mut tasks[current] {
        let expired = task.futex_timed_out;
        task.futex_timed_out = false;
        expired
    } else { false }
}

/// Real `FUTEX_WAKE`: flips up to `max` tasks currently `Blocked` on
/// `addr` back to `Ready`, and returns how many it actually woke (what a
/// real `FUTEX_WAKE`'s return value is) -- `linux_syscall.rs::sys_futex`
/// hands that straight back to the caller. Waking doesn't itself trigger
/// an immediate context switch; a woken task just becomes eligible again
/// and picks up its next turn the ordinary way, through `schedule`'s
/// normal round-robin selection (the next timer tick, or the very next
/// voluntary `schedule()` call -- for instance the one at the end of
/// whatever syscall the waking task itself eventually returns through).
pub fn futex_wake(addr: u64, max: u32) -> u32 {
    let mut tasks = TASKS.lock();
    let mut woken = 0u32;
    for slot in tasks.iter_mut() {
        if woken >= max {
            break;
        }
        if let Some(task) = slot {
            if task.state == TaskState::Blocked && task.futex_addr == Some(addr) {
                task.state = TaskState::Ready;
                task.futex_addr = None;
                task.wait_deadline = None;
                task.futex_timed_out = false;
                woken += 1;
            }
        }
    }
    woken
}

/// The current task's `brk` -- for `syscall.rs`'s `SYS_BRK`. See
/// `Task::heap_end`'s doc comment.
pub fn heap_end() -> u64 {
    let tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    tasks[current].as_ref().map(|t| t.heap_end).unwrap_or(USER_HEAP_BASE)
}

/// Updates the current task's `brk` after `SYS_BRK` has mapped (or failed
/// to map) new pages to cover it.
pub fn set_heap_end(new_end: u64) {
    let mut tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    if let Some(task) = &mut tasks[current] {
        task.heap_end = new_end;
    }
}

/// The currently running task's own ID -- what `linux_syscall.rs`'s
/// `set_tid_address` hands back (see its doc comment for why "this
/// task's own `Task::id`" is an honest enough stand-in for a real Linux
/// thread ID here).
pub fn current_id() -> u64 {
    let tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    tasks[current].as_ref().map(|t| t.id).unwrap_or(0)
}

/// The currently running task's own `cr3` -- what a fatal-fault handler
/// needs to find and kill the rest of its thread group (see [`kill_group`]).
pub fn current_cr3() -> u64 {
    let tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    tasks[current].as_ref().map(|t| t.cr3).unwrap_or(0)
}

/// Marks every *other* task sharing this `cr3` as `Terminated` -- real
/// Linux's fatal-signal-kills-the-whole-process semantics (an uncaught
/// `#GP`/`#PF` is the kernel-side equivalent of an uncatchable `SIGSEGV`),
/// which plain per-task [`task_exit`] doesn't implement on its own. Without
/// this, a `clone3`'d sibling thread that outlives its dead thread-group
/// leader (see README item 40's own discovery of exactly this happening
/// with real HotSpot) keeps this `cr3` looking "shared" forever, which
/// makes `schedule`'s reaping sweep permanently defer ever calling
/// `paging::destroy_address_space` on it -- a real memory leak (see item
/// 42), not just an orphaned thread that would otherwise eventually exit.
/// `except` is the faulting task's own ID, already being terminated by its
/// own call to [`task_exit`] right after this -- left alone here so as not
/// to double-handle it.
pub fn kill_group(cr3: u64, except: u64) {
    let mut tasks = TASKS.lock();
    for slot in tasks.iter_mut() {
        if let Some(task) = slot {
            if task.id != except && task.cr3 == cr3 && task.state != TaskState::Terminated {
                task.state = TaskState::Terminated;
            }
        }
    }
}

/// The current task's `FS_BASE`, as last set by [`set_fs_base`] -- what
/// `linux_syscall.rs`'s `arch_prctl(ARCH_GET_FS, ...)` reads back.
pub fn fs_base() -> u64 {
    let tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    tasks[current].as_ref().map(|t| t.fs_base).unwrap_or(0)
}

/// Sets the current task's `FS_BASE` -- what `linux_syscall.rs`'s
/// `arch_prctl(ARCH_SET_FS, ...)` calls -- and, since this task is by
/// definition the one currently running (nothing else can call this on
/// its own behalf), also loads the real `FS_BASE` MSR immediately rather
/// than waiting for the next `schedule` to do it. Without that immediate
/// load, every `%fs:`-relative access between this call returning and the
/// next context switch would still read through the *old* base, which
/// for a task setting it up for the first time is 0 -- exactly the
/// startup crash this exists to avoid.
pub fn set_fs_base(value: u64) {
    let mut tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    if let Some(task) = &mut tasks[current] {
        task.fs_base = value;
    }
    drop(tasks);
    const IA32_FS_BASE: u32 = 0xC000_0100;
    unsafe {
        crate::gdt::wrmsr(IA32_FS_BASE, value);
    }
}

/// Retain the current table briefly under TASKS, without nesting file locks.
///
/// # Panics
/// If there's no current task, which should be unreachable outside early
/// boot before `init()` -- every syscall this backs only ever runs from
/// inside a task that's actually executing.
fn current_open_files() -> Arc<IrqSpinLock<[Option<OpenFile>; MAX_OPEN_FILES]>> {
    let tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    Arc::clone(&tasks[current].as_ref().expect("open files: no current task").open_files)
}

/// Short metadata-only access to the current descriptor table. Do not schedule,
/// perform I/O, or access user memory inside f. Linux clones share this table;
/// unrelated processes/native tasks retain independent tables.
pub fn with_current_open_files<R>(f: impl FnOnce(&mut [Option<OpenFile>; MAX_OPEN_FILES]) -> R) -> R {
    let files = current_open_files(); // TASKS is released before taking the file lock.
    let mut table = files.lock();
    f(&mut table)
}

/// Stage a bounded read into resident kernel memory, outside TASKS. Callers
/// copy to user memory only after this returns. Explicit offsets preserve pos.
pub fn read_open_file(fd: usize, buffer: &mut [u8], offset: Option<usize>) -> Result<usize, i64> {
    // A `Dir`/`Writable` fd (see `OpenFile::extra`'s doc comment) is pure
    // in-memory bookkeeping, not disk I/O -- handled entirely in one lock
    // acquisition, no need for the release-the-lock-across-I/O dance the
    // `FileBacking::Disk` path below still needs.
    let fast: Option<Result<usize, i64>> = with_current_open_files(|table| {
        let file = table.get_mut(fd).and_then(|f| f.as_mut())?;
        let (start, n) = match file.extra.as_deref() {
            Some(OpenExtra::Dir(_)) => return Some(Err(-21)), // EISDIR: a real plain read() on a directory fd
            Some(OpenExtra::Writable(_, buf)) => {
                let start = offset.unwrap_or(file.pos);
                let n = (buf.len() as u64).saturating_sub(start as u64).min(buffer.len() as u64) as usize;
                if n != 0 { buffer[..n].copy_from_slice(&buf[start..start + n]); }
                (start, n)
            }
            None => return None,
        };
        if offset.is_none() { file.pos = start + n; }
        Some(Ok(n))
    });
    if let Some(result) = fast {
        return result;
    }

    let (mut backing, start) = with_current_open_files(|table| {
        let file = table.get(fd).and_then(|f| f.as_ref()).ok_or(-9i64)?;
        Ok::<_, i64>((file.data, offset.unwrap_or(file.pos)))
    })?;
    // No TASKS guard across disk I/O, and never a user pointer in this helper.
    // Syscall IF=0 on one CPU keeps the fd and offset stable between these
    // short locks. No scheduling or user access is permitted in this helper.
    let n = backing.read_at(start as u64, buffer)?;
    with_current_open_files(|table| {
        if let Some(file) = table.get_mut(fd).and_then(|f| f.as_mut()) {
            file.data = backing;
            if offset.is_none() { file.pos = start + n; }
        }
    });
    Ok(n)
}

/// Real Linux `write`/`pwrite`-style write onto a real [`OpenExtra::Writable`]
/// fd -- see its own doc comment. Extends the in-memory buffer with zero
/// bytes if `offset` (or the fd's own sequential position) lands past the
/// current end, the same "seek past EOF, then write, leaves a real hole"
/// semantics `sys_lseek`'s own doc comment already describes for reads.
/// Any other fd kind (`Dir`, or a plain read-only `FileBacking`) honestly
/// refuses with `EBADF` -- this kernel's FAT16 write path
/// (`fat16::write_file`) is whole-buffer, so there's no real way to grow
/// an existing read-only lazy `FileBacking::Disk` read into a writable one
/// without re-opening it, and nothing has ever needed that yet.
pub fn write_open_file(fd: usize, bytes: &[u8], offset: Option<usize>) -> Result<usize, i64> {
    with_current_open_files(|table| {
        let file = table.get_mut(fd).and_then(|f| f.as_mut()).ok_or(-9i64)?;
        let Some(OpenExtra::Writable(_, buf)) = file.extra.as_deref_mut() else {
            return Err(-9); // EBADF
        };
        let start = offset.unwrap_or(file.pos);
        if buf.len() < start {
            buf.resize(start, 0);
        }
        let end = start + bytes.len();
        if buf.len() < end {
            buf.resize(end, 0);
        }
        buf[start..end].copy_from_slice(bytes);
        if offset.is_none() { file.pos = end; }
        Ok(bytes.len())
    })
}

/// Real Linux `ftruncate(2)` onto a real [`OpenExtra::Writable`] fd --
/// grows (zero-filled) or shrinks the in-memory buffer to exactly `size`,
/// same real semantics a real regular file's `ftruncate` has. Any other
/// fd kind honestly refuses with `EINVAL`, real Linux's own answer for
/// `ftruncate` on something that isn't a regular writable file.
pub fn truncate_open_file(fd: usize, size: u64) -> Result<(), i64> {
    with_current_open_files(|table| {
        let file = table.get_mut(fd).and_then(|f| f.as_mut()).ok_or(-9i64)?;
        let Some(OpenExtra::Writable(_, buf)) = file.extra.as_deref_mut() else {
            return Err(-22); // EINVAL
        };
        buf.resize(size as usize, 0);
        Ok(())
    })
}

/// `(size, ino, is_dir)` for `linux_syscall.rs`'s `sys_fstat`/`sys_lseek`
/// -- checks `extra` first (a `Dir`/`Writable` fd's real size/kind lives
/// there, not in the inert `FileBacking::Static(&[])` placeholder `data`
/// holds for either -- see `OpenFile::extra`'s own doc comment), falling
/// back to the ordinary `FileBacking` case otherwise.
pub fn open_file_len(fd: usize) -> Option<(u64, u64, bool)> {
    with_current_open_files(|table| {
        let file = table.get(fd).and_then(|f| f.as_ref())?;
        let (size, is_dir) = match file.extra.as_deref() {
            Some(OpenExtra::Dir(_)) => (0u64, true),
            Some(OpenExtra::Writable(_, buf)) => (buf.len() as u64, false),
            None => (file.data.len() as u64, false),
        };
        Some((size, file.ino, is_dir))
    })
}

/// The real, already-resolved absolute path an open directory fd names --
/// what `linux_syscall.rs`'s `sys_fchdir` needs to call `fat16::change_dir`
/// with. `None` for anything that isn't a real open [`OpenExtra::Dir`].
pub fn open_file_dir_path(fd: usize) -> Option<String> {
    with_current_open_files(|table| {
        let file = table.get(fd).and_then(|f| f.as_ref())?;
        match file.extra.as_deref() {
            Some(OpenExtra::Dir(path)) => Some(path.clone()),
            _ => None,
        }
    })
}

/// The current task's real invocation path -- see [`Task::exe_path`]'s doc
/// comment. Empty for anything that didn't go through `spawn_user`/
/// `spawn_clone_raw` with a real one (every kernel thread, and
/// `usermode.rs`'s own hand-written ring-3 demo, which has no real file
/// behind it at all).
pub fn current_exe_path() -> String {
    let tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    tasks[current].as_ref().map(|t| t.exe_path.clone()).unwrap_or_default()
}

/// Installs the current task's real `rt_sigaction`-provided handler for
/// `sig` (`(handler, flags, restorer)`, straight off the raw
/// `struct k_sigaction` `linux_syscall.rs`'s `sys_rt_sigaction` already
/// parsed -- see its doc comment). Silently ignored for an out-of-range
/// signal number, same "don't crash the kernel over a guest's bad
/// argument" spirit as every other syscall argument here.
pub fn set_sigaction(sig: u64, handler: u64, flags: u64, restorer: u64) {
    if sig == 0 || sig as usize >= NSIG {
        return;
    }
    let mut tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    if let Some(task) = &mut tasks[current] {
        task.sig_handlers[sig as usize] = (handler, flags, restorer);
    }
}

/// The current task's raw, as-installed `(handler, flags, restorer)` for
/// `sig` -- `(0, 0, 0)` (`SIG_DFL`, no restorer) if nothing's ever called
/// `rt_sigaction` for it. What `linux_syscall.rs`'s `sys_rt_sigaction`
/// reads back for a real `oldact` argument, and what
/// `paging::handle_page_fault` consults (with its own `handler > 1` /
/// `SA_RESTORER` checks -- `1` is `SIG_IGN`, not a real handler address)
/// to decide whether a fatal fault has anywhere real to go.
pub fn sigaction(sig: u64) -> (u64, u64, u64) {
    if sig == 0 || sig as usize >= NSIG {
        return (0, 0, 0);
    }
    let tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    tasks[current].as_ref().map(|t| t.sig_handlers[sig as usize]).unwrap_or((0, 0, 0))
}

/// Whether the *currently running* task is already inside a real
/// delivered signal handler for exactly `sig` -- see
/// [`Task::sig_delivering`]'s doc comment for why a fault that recurs
/// while this is true must *not* try to deliver the same signal again.
pub fn is_delivering_signal(sig: u8) -> bool {
    let tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    tasks[current].as_ref().map(|t| t.sig_delivering == Some(sig)).unwrap_or(false)
}

/// Marks the current task as now running inside a real handler for `sig`
/// and stashes `ctx` (the full pre-signal CPU state) for a later
/// `rt_sigreturn` to restore. Returns `false` (and stashes nothing) if
/// this task is already delivering some signal -- see
/// [`is_delivering_signal`]; the caller (`paging::handle_page_fault`) is
/// expected to treat that as "can't deliver, fall through to the fatal
/// path" rather than clobbering an in-flight delivery's saved context.
pub fn begin_signal_delivery(sig: u8, ctx: SavedContext) -> bool {
    let mut tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    let Some(task) = &mut tasks[current] else { return false };
    if task.sig_delivering.is_some() {
        return false;
    }
    task.sig_delivering = Some(sig);
    task.sig_saved_ctx = Some(ctx);
    true
}

/// The other half of [`begin_signal_delivery`]: called by
/// `linux_syscall.rs`'s `sys_rt_sigreturn` once the handler calls its
/// restorer trampoline. Clears `sig_delivering` (so a *future* fault can
/// deliver a fresh signal again) and hands back the saved context to
/// restore, or `None` if this task wasn't actually delivering one --
/// `sys_rt_sigreturn` treats that as the same kind of "the guest did
/// something it had no business doing" case every other malformed-input
/// syscall argument here does.
pub fn end_signal_delivery() -> Option<SavedContext> {
    let mut tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    let task = tasks[current].as_mut()?;
    task.sig_delivering = None;
    task.sig_saved_ctx.take()
}

/// For the `ps` shell command: `(id, name, state, ticks_run, is_current)`
/// for every live task slot.
pub fn list() -> Vec<(u64, &'static str, TaskState, u64, bool)> {
    let tasks = TASKS.lock();
    let current = CURRENT.load(Ordering::Relaxed);
    tasks
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.as_ref().map(|t| (t.id, t.name, t.state, t.ticks_run, i == current)))
        .collect()
}
