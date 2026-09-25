//! A second, *parallel* syscall gate: the real x86_64 Linux `syscall`/
//! `sysretq` instruction pair, real Linux syscall numbers, and real Linux
//! calling/error conventions (a negative return means `-errno`, not a
//! sentinel like `syscall.rs`'s `SYS_ERROR`). `syscall.rs`'s `int 0x80`
//! gate still exists and still backs every native KonjacOS demo program
//! unchanged -- this is strictly additive, a second door into the same
//! kernel, for a different kind of guest: an actual Linux userspace
//! binary (to start, one built with `musl-gcc`; see `README.md`'s
//! roadmap and `userprogs/`) that was never written with KonjacOS's own
//! tiny ABI in mind and has no way to know it exists.
//!
//! ## Why this needs a genuinely different entry mechanism
//!
//! `int 0x80` is an *interrupt gate*: the CPU consults the IDT, and for a
//! ring3->ring0 transition, automatically switches to the TSS's RSP0
//! before pushing anything -- see `syscall.rs`'s module docs and
//! `gdt::set_kernel_stack`. `syscall` is not an interrupt at all; it's a
//! dedicated instruction with its own MSR-configured behavior (`STAR`/
//! `LSTAR`/`SFMASK`, enabled by `EFER.SCE`), and critically, it performs
//! **no stack switch whatsoever** -- `RSP` on entry is still whatever the
//! calling ring-3 program's stack pointer was. `RCX`/`R11` get repurposed
//! by the CPU itself to stash the return `RIP`/`RFLAGS` (which is also
//! why the real Linux syscall convention passes its 4th argument in `R10`
//! instead of `RCX` -- `RCX` isn't available). So the very first thing
//! [`linux_syscall_entry`] has to do, before it's safe to push a single
//! byte, is find and switch to a real kernel stack itself -- see
//! `gdt::SYSCALL_KERNEL_RSP` and the asm's own comments for exactly how.
//!
//! ## Why this needs `gdt.rs`'s second user code/data pair
//!
//! `sysretq`'s half of the `STAR` MSR is hard-wired to a fixed segment
//! layout (`SS` at `STAR[63:48]+8`, `CS` at `+16`) that's the *opposite*
//! order from `USER_CODE_SELECTOR`/`USER_DATA_SELECTOR` (code, then
//! data) -- see `gdt::SYSRET_USER_DATA_SELECTOR`'s doc comment for the
//! full reasoning. Nothing about the descriptors themselves differs; it's
//! purely an ordering constraint.
//!
//! ## What's actually implemented
//!
//! Only the handful of real Linux syscalls a minimal, *statically*
//! linked `musl-gcc`-built program needs to get from `_start` through
//! `main` to `exit`, determined empirically (`strace -f` against a real
//! musl `hello.c`, run natively on the same x86_64 Linux this kernel's
//! toolchain already lives on -- ground truth, not a guess at what musl
//! "probably" calls): `arch_prctl` (TLS setup -- see `task::set_fs_base`),
//! `set_tid_address`, `ioctl` (just enough to answer `TIOCGWINSZ` the way
//! a non-tty stream honestly would: `-ENOTTY`), `writev` (real stdio
//! buffering flushes through this, not plain `write`), `brk`, `mmap`/
//! `munmap` (reusing the exact same reservation scheme as `syscall.rs`'s
//! `SYS_MMAP`), and `exit`/`exit_group`. `read`/`write`/`close` are also
//! wired up, reusing `task`'s existing per-task open-file table, for
//! whatever a slightly less minimal program needs next. Everything else
//! returns `-ENOSYS`, loudly logged, instead of silently pretending to
//! succeed -- the same "honest, specific refusal instead of undefined
//! behavior" discipline `loader.rs` already practices for formats/
//! relocations it can't handle.

extern crate alloc;

use core::arch::global_asm;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::fat16;
use crate::gdt;
use crate::paging;
use crate::pmm;
use crate::println;
use crate::task;
use crate::task::{OpenFile, FileBacking};
use crate::timer;

// Real Linux x86_64 syscall numbers (see the kernel's own
// arch/x86/entry/syscalls/syscall_64.tbl) -- unlike syscall.rs's table,
// these aren't KonjacOS's own invention, so they're spelled out with
// their real numeric values rather than assigned by `enum` order, and
// deliberately sparse (only what's implemented gets a name).
const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_PREAD64: u64 = 17;
const SYS_OPEN: u64 = 2;
const SYS_MKDIR: u64 = 83;
const SYS_UNLINK: u64 = 87;
const SYS_UNLINKAT: u64 = 263;
const SYS_MKDIRAT: u64 = 258;
const SYS_FLOCK: u64 = 73;
const SYS_FCHDIR: u64 = 81;
const SYS_FTRUNCATE: u64 = 77;
const SYS_CLOSE: u64 = 3;
const SYS_FSTAT: u64 = 5;
const SYS_MMAP: u64 = 9;
const SYS_MPROTECT: u64 = 10;
const SYS_MUNMAP: u64 = 11;
const SYS_BRK: u64 = 12;
const SYS_RT_SIGACTION: u64 = 13;
const SYS_RT_SIGPROCMASK: u64 = 14;
const SYS_RT_SIGRETURN: u64 = 15;
const SYS_IOCTL: u64 = 16;
const SYS_WRITEV: u64 = 20;
const SYS_FCNTL: u64 = 72;
const SYS_GETCWD: u64 = 79;
const SYS_CLONE: u64 = 56;
const SYS_CLONE3: u64 = 435;
const SYS_EXIT: u64 = 60;
const SYS_ARCH_PRCTL: u64 = 158;
const SYS_SET_TID_ADDRESS: u64 = 218;
const SYS_EXIT_GROUP: u64 = 231;
const SYS_FUTEX: u64 = 202;
const SYS_LSEEK: u64 = 8;
const SYS_OPENAT: u64 = 257;
const SYS_NEWFSTATAT: u64 = 262;
const SYS_CLOCK_GETTIME: u64 = 228;
const SYS_NANOSLEEP: u64 = 35;
const SYS_SCHED_YIELD: u64 = 24;
const SYS_CLOCK_NANOSLEEP: u64 = 230;
const SYS_GETRANDOM: u64 = 318;
const SYS_READLINK: u64 = 89;
const SYS_READLINKAT: u64 = 267;
const SYS_ACCESS: u64 = 21;
const SYS_GETPID: u64 = 39;
const SYS_GETTID: u64 = 186;
const SYS_SCHED_GETAFFINITY: u64 = 204;
const SYS_GETCPU: u64 = 309;
const SYS_UNAME: u64 = 63;
const SYS_MADVISE: u64 = 28;
const SYS_SYSINFO: u64 = 99;
const SYS_GETTIMEOFDAY: u64 = 96;
const SYS_CLOCK_GETRES: u64 = 229;
const SYS_GETUID: u64 = 102;
const SYS_GETGID: u64 = 104;
const SYS_GETEUID: u64 = 107;
const SYS_GETEGID: u64 = 108;
const SYS_PRCTL: u64 = 157;
const SYS_SET_ROBUST_LIST: u64 = 273;
const SYS_PRLIMIT64: u64 = 302;
const SYS_RSEQ: u64 = 334;

// Real Linux clock IDs sys_clock_gettime actually answers -- see its own
// doc comment for why all three collapse to the same "ticks since boot"
// source.
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_BOOTTIME: u64 = 7;

// Real Linux clone(2) flag bits actually consulted by sys_clone below --
// see its doc comment for what each one does here.
const CLONE_SETTLS: u64 = 0x0008_0000;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;

// Real Linux futex(2) op bits actually consulted by sys_futex below.
const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_CLOCK_REALTIME: u64 = 0x100;
const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;
const FUTEX_PRIVATE_FLAG: u64 = 0x80;

const ARCH_SET_FS: u64 = 0x1002;
const ARCH_GET_FS: u64 = 0x1003;

const TIOCGWINSZ: u64 = 0x5413;

// Real `mmap(2)` flag/prot bits actually consulted below -- see
// `sys_mmap`'s doc comment for why only these matter here.
const MAP_ANONYMOUS: u64 = 0x20;

// Real Linux open(2) flag bits sys_open actually consults -- see its own
// doc comment.
const O_ACCMODE: u64 = 0x3;
const O_CREAT: u64 = 0o100;
const O_DIRECTORY: u64 = 0o200000;

// Real Linux errno values (negated on return -- see the module docs).
const ENOSYS: i64 = -38;
const ENOTTY: i64 = -25;
const EINVAL: i64 = -22;
const EBADF: i64 = -9;
const EAGAIN: i64 = -11;
const ETIMEDOUT: i64 = -110;
const ENOENT: i64 = -2;
const EEXIST: i64 = -17;
const ENOTDIR: i64 = -20;
const EISDIR: i64 = -21;
const ENOMEM: i64 = -12;

// Real Linux RLIMIT_* resource numbers sys_prlimit64 actually special-cases
// below -- see its own doc comment.
const RLIMIT_STACK: u64 = 3;
const RLIMIT_NOFILE: u64 = 7;
const RLIM_INFINITY: u64 = u64::MAX;

/// A real Linux `struct stat` (x86_64, musl and glibc layout match) is
/// 144 bytes -- see `sys_fstat`'s doc comment for exactly which fields
/// this kernel actually fills in.
const STAT_SIZE: usize = 144;

const PAGE_SIZE: u64 = 4096;
const MAX_IO_LEN: usize = 4096;
/// A real `writev`'s `iovcnt` is trusted the same limited way every other
/// pointer/length argument from ring 3 is (see `syscall.rs`'s module
/// docs) -- this just keeps a bogus count from making the kernel walk an
/// unbounded array hunting for `iovec`s that aren't there.
const MAX_IOVEC: usize = 64;
/// A real `open`'s path is a NUL-terminated string of unbounded length in
/// principle; trusted the same limited way as every other pointer this
/// kernel reads from ring 3, capped so a missing NUL can't make
/// `sys_open`'s scan run away -- see `syscall.rs`'s own `MAX_PATH_LEN` for
/// the equivalent limit on KonjacOS's own ABI.
const MAX_PATH_LEN: usize = 256;

/// # Safety
/// Must be called once, after `gdt::init()` and `task::init()` (context
/// switches need to be able to reach a valid kernel stack/FS_BASE the
/// moment a `syscall` first lands), and before `sti`.
pub unsafe fn init() {
    unsafe extern "C" {
        fn linux_syscall_entry();
    }

    const IA32_EFER: u32 = 0xC000_0080;
    const IA32_STAR: u32 = 0xC000_0081;
    const IA32_LSTAR: u32 = 0xC000_0082;
    const IA32_SFMASK: u32 = 0xC000_0084;
    const EFER_SCE: u64 = 1 << 0;

    unsafe {
        let efer = rdmsr(IA32_EFER);
        gdt::wrmsr(IA32_EFER, efer | EFER_SCE);

        // High 32 bits: [63:48] = sysret base (gdt::SYSRET_USER_DATA_SELECTOR
        // minus its own RPL/+8 offset -- see that constant's doc comment),
        // [47:32] = syscall entry CS (kernel code; SS is implicitly this +8,
        // which is exactly gdt::KERNEL_DATA_SELECTOR). Low 32 bits are only
        // consulted for a 32-bit `syscall`, which nothing here ever issues.
        let sysret_base = (gdt::SYSRET_USER_DATA_SELECTOR & !3) - 8;
        let star = ((sysret_base as u64) << 48) | ((gdt::KERNEL_CODE_SELECTOR as u64) << 32);
        gdt::wrmsr(IA32_STAR, star);

        gdt::wrmsr(IA32_LSTAR, linux_syscall_entry as *const () as u64);

        // Cleared from RFLAGS on entry, same spirit as an interrupt gate
        // implicitly clearing IF: IF (bit 9, so a nested syscall/IRQ can't
        // land on this task's kernel stack mid-switch -- see
        // linux_syscall_entry's own comments), TF (bit 8, no single-step
        // surprises), and DF (bit 10, so string instructions in the
        // handler don't inherit direction flag baggage from ring-3 code).
        const RFLAGS_IF: u64 = 1 << 9;
        const RFLAGS_TF: u64 = 1 << 8;
        const RFLAGS_DF: u64 = 1 << 10;
        gdt::wrmsr(IA32_SFMASK, RFLAGS_IF | RFLAGS_TF | RFLAGS_DF);
    }
}

// Tripwire, same spirit as task.rs's enter_user_mode selector asserts:
// confirms sysret_base+16 (what `init` actually programs into STAR, with
// RPL folded in by the CPU on the way out) lands on the exact selector
// gdt.rs already named for it, so the two files can't silently drift
// apart.
const _: () = assert!(((gdt::SYSRET_USER_DATA_SELECTOR & !3) - 8 + 16) | 3 == gdt::SYSRET_USER_CODE_SELECTOR, "update linux_syscall::init's STAR computation");

unsafe fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nostack, preserves_flags));
    }
    ((high as u64) << 32) | low as u64
}

/// Called by `linux_syscall_entry` for every real `syscall` instruction.
/// Takes/returns raw `u64`s the same way `syscall.rs::syscall_handler`
/// does; callers that want real Linux `-errno` semantics just bit-cast a
/// negative `i64` in (see every handler below).
#[unsafe(no_mangle)]
extern "C" fn linux_syscall_handler(number: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    (match number {
        SYS_ARCH_PRCTL => sys_arch_prctl(a0, a1),
        SYS_SET_TID_ADDRESS => sys_set_tid_address(),
        SYS_IOCTL => sys_ioctl(a0, a1),
        SYS_WRITEV => sys_writev(a0, a1, a2),
        SYS_WRITE => sys_write(a0, a1, a2),
        SYS_READ => sys_read(a0, a1, a2),
        SYS_GETCWD => sys_getcwd(a0, a1),
        SYS_PREAD64 => sys_pread64(a0, a1, a2, a3),
        SYS_LSEEK => sys_lseek(a0, a1, a2),
        SYS_CLOSE => sys_close(a0),
        SYS_BRK => sys_brk(a0) as i64,
        SYS_OPEN => sys_open(a0, a1, a2),
        SYS_FSTAT => sys_fstat(a0, a1),
        // openat(dirfd, path, flags, mode): dirfd/flags/mode all read but
        // unconsulted -- see sys_newfstatat's doc comment just below for
        // why a dirfd can't actually mean anything different here, and
        // sys_open's own doc comment for flags/mode. Genuinely the same
        // operation as sys_open once that's true, not just similar, so it
        // reuses it outright rather than duplicating its body.
        SYS_OPENAT => sys_open(a1, a2, a3),
        SYS_NEWFSTATAT => sys_newfstatat(a1, a2),
        SYS_MKDIR => sys_mkdir(a0),
        // mkdirat(dirfd, path, mode): dirfd ignored, same precedent as
        // SYS_OPENAT reusing sys_open above -- every path this kernel has
        // ever been asked to create has been absolute.
        SYS_MKDIRAT => sys_mkdir(a1),
        SYS_UNLINK => sys_unlink(a0),
        // unlinkat(dirfd, path, flags): dirfd ignored, same precedent as
        // openat/mkdirat above -- every path here has been absolute.
        // AT_REMOVEDIR (flag 0x200) would mean "this is really an rmdir"
        // on real Linux; not observed here, and fat16::remove_file
        // already honestly refuses a directory target regardless.
        SYS_UNLINKAT => sys_unlink(a1),
        // flock: advisory, cooperative locking against *other processes*
        // -- there's never a second process here to contend with, so
        // accepting it as a no-op is honest, not a shortcut around real
        // contention this kernel can't detect.
        SYS_FLOCK => 0,
        SYS_FCHDIR => sys_fchdir(a0),
        SYS_FTRUNCATE => sys_ftruncate(a0, a1),
        SYS_CLOCK_GETTIME => sys_clock_gettime(a0, a1),
        SYS_NANOSLEEP => sys_clock_nanosleep(CLOCK_MONOTONIC, 0, a0),
        SYS_SCHED_YIELD => { task::yield_now(); 0 },
        SYS_CLOCK_NANOSLEEP => sys_clock_nanosleep(a0, a1, a2),
        SYS_GETRANDOM => sys_getrandom(a0, a1),
        SYS_READLINK => sys_readlink(a0, a1, a2),
        // readlinkat(dirfd, path, buf, bufsiz): dirfd ignored, same reason
        // and same precedent as SYS_OPENAT reusing sys_open -- every path
        // this loader/ld.so has ever asked about here is absolute. Real
        // ld.so's own `_dl_get_origin` (glibc, modern versions) calls this
        // instead of plain `readlink` to resolve `/proc/self/exe` for a
        // `$ORIGIN` in its own RPATH -- found the exact same ground-truth
        // way as every other syscall in this module: it's what actually
        // showed up trying to load a real, unmodified `java` binary (see
        // item 33's own `sys_readlink` doc comment and this item's README
        // entry).
        SYS_READLINKAT => sys_readlink(a1, a2, a3),
        SYS_ACCESS => sys_access(a0),
        // getpid/gettid: this kernel has no distinction between a process
        // and one of its threads the way real Linux's (task_struct's own
        // tgid vs pid) does -- every task, `clone`d thread or not, already
        // gets its own unique, real `task::current_id()` (see task.rs's
        // module docs on process-vs-thread) -- so both real syscalls
        // honestly answer with the same real, unique-per-task value.
        // Real Linux would have every thread in a process share one PID
        // (getpid) while each still has its own distinct TID (gettid);
        // collapsing that distinction is a real, narrower guarantee than
        // Linux gives (two threads here are more distinguishable than real
        // Linux threads are), not a wrong one -- nothing that's run here
        // yet has depended on two threads actually sharing a getpid value.
        SYS_GETPID => task::current_id() as i64,
        SYS_GETTID => task::current_id() as i64,
        SYS_SCHED_GETAFFINITY => sys_sched_getaffinity(a1, a2),
        SYS_GETCPU => sys_getcpu(a0, a1),
        SYS_UNAME => sys_uname(a0),
        // madvise: purely advisory on real Linux too (a hint the kernel is
        // always free to ignore) -- honestly a no-op here rather than
        // implementing any of the specific hints (MADV_DONTNEED,
        // MADV_WILLNEED, ...), since nothing this kernel does with a
        // mapped page's contents actually depends on any of them yet.
        SYS_MADVISE => 0,
        SYS_SYSINFO => sys_sysinfo(a0),
        SYS_GETTIMEOFDAY => sys_gettimeofday(a0, a1),
        SYS_CLOCK_GETRES => sys_clock_getres(a0, a1),
        // This kernel has no real user/group model (every task already
        // runs with full kernel privilege -- see `apex.rs` for the one
        // place that distinction is emulated at all) -- root (0) for all
        // four is the same honest answer `loader.rs`'s own auxv
        // AT_UID/AT_EUID/AT_GID/AT_EGID entries already give a spawned
        // program, not a fabricated non-zero identity this kernel can't
        // back up with real permission checks.
        SYS_GETUID => 0,
        SYS_GETEUID => 0,
        SYS_GETGID => 0,
        SYS_GETEGID => 0,
        SYS_PRCTL => sys_prctl(a0),
        // set_robust_list: accepted, not acted on. Real Linux uses the
        // registered list to clean up a dead thread's held futexes for
        // other threads waiting on them; this kernel's own futex/task-
        // death paths don't consult it, the same honestly-scoped gap
        // `sys_futex`'s own doc comment already admits for cross-task
        // robustness. Every glibc thread startup calls this once,
        // unconditionally -- refusing it outright would make ordinary
        // thread creation noisy for no benefit.
        SYS_SET_ROBUST_LIST => 0,
        SYS_PRLIMIT64 => sys_prlimit64(a1, a2, a3),
        // rseq: real restartable-sequence support needs this kernel's own
        // scheduler to keep a per-task `struct rseq`'s `cpu_id` field
        // updated across every context switch -- not implemented. Real
        // glibc (since 2.35) already treats a failed registration as
        // "unavailable, don't use it" and falls back cleanly -- explicit
        // `ENOSYS` here is the same honest refusal the catch-all below
        // already gave it, just named instead of anonymous, so it doesn't
        // spam the console once per thread.
        SYS_RSEQ => ENOSYS,
        SYS_FCNTL => 0, // accepted no-op -- see the module docs; only ever seen used for FD_CLOEXEC bookkeeping this kernel doesn't need (every fd is already private per-task, see task::OpenFile).
        SYS_MMAP => sys_mmap(a0, a1, a2, a3, a4),
        SYS_MPROTECT => sys_mprotect(a0, a1, a2),
        SYS_MUNMAP => sys_munmap(a0, a1),
        SYS_CLONE => sys_clone(a0, a1, a2, a3, a4),
        SYS_CLONE3 => sys_clone3(a0, a1),
        SYS_FUTEX => sys_futex(a0, a1, a2, a3),
        SYS_RT_SIGACTION => sys_rt_sigaction(a0, a1, a2),
        // Real signal *masking* (blocking/unblocking specific signals via
        // rt_sigprocmask) still isn't implemented -- every program so far
        // only probes/sets it around a handler this kernel now genuinely
        // does deliver (see item 25's README entry), never actually
        // depends on a signal staying blocked. Honestly scoped the same
        // way sys_futex/spawn_thread's own doc comments admit their gaps,
        // not silently pretended away.
        SYS_RT_SIGPROCMASK => 0,
        SYS_RT_SIGRETURN => sys_rt_sigreturn(),
        SYS_EXIT | SYS_EXIT_GROUP => {
            // Never returns.
            task::task_exit();
        }
        _ => {
            println!("linux_syscall: unimplemented syscall number {number} (a0={a0:#x} a1={a1:#x} a2={a2:#x} a3={a3:#x} a4={a4:#x})");
            ENOSYS
        }
    }) as u64
}

fn sys_arch_prctl(code: u64, addr: u64) -> i64 {
    match code {
        ARCH_SET_FS => {
            task::set_fs_base(addr);
            0
        }
        ARCH_GET_FS => {
            // Trusted pointer, same as every other syscall argument here
            // (see the module docs) -- a real implementation would check
            // `addr` is actually mapped writable in the caller's own
            // address space first.
            unsafe { (addr as *mut u64).write(task::fs_base()) };
            0
        }
        _ => EINVAL,
    }
}

fn sys_set_tid_address() -> i64 {
    // Real Linux returns the caller's thread ID. KonjacOS has no separate
    // thread-ID/process-ID distinction (see task.rs's module docs -- a
    // `spawn_thread`'d task just has its own `Task::id`), so this hands
    // back the current task's own ID, which is honestly what a
    // single-threaded caller (the only kind that exists so far) actually
    // wants to see here anyway.
    task::current_id() as i64
}

fn sys_ioctl(fd: u64, request: u64) -> i64 {
    match request {
        TIOCGWINSZ => ENOTTY, // exactly what a real, non-tty stdout answers -- see the module docs' strace-derived rationale.
        _ => {
            let _ = fd;
            ENOSYS
        }
    }
}

/// Writes `text` to the console the same way `syscall.rs::SYS_WRITE`
/// does -- fd isn't checked against anything real (no real stdout/stderr
/// distinction exists yet), just trusted to be a small integer a caller
/// meant as "the console", matching this kernel's only actual output
/// sink.
fn write_bytes_to_console(bytes: &[u8]) {
    match core::str::from_utf8(bytes) {
        Ok(text) => crate::print!("{text}"),
        Err(_) => println!("linux_syscall: write: not valid UTF-8 ({} bytes)", bytes.len()),
    }
}

/// `fd` 0/1/2 (stdin/stdout/stderr) always go to the real console --
/// never real table entries (nothing here ever calls `open` and gets one
/// of those three numbers back low enough to collide, since `sys_open`'s
/// own fd search starts at the table's first free slot, but a real
/// program's `write(1, ...)`/`write(2, ...)` use the fixed numbers
/// directly, inherited rather than opened). Any other `fd` now really
/// means a real open file (see `task::OpenFile::extra`'s doc comment for
/// how `java -version`'s own `hsperfdata` PerfData file first needed
/// this) -- routed to `task::write_open_file` instead of the console,
/// which was this function's *only* behavior before, regardless of `fd`,
/// simply because nothing had ever opened a real file for writing yet.
fn sys_write(fd: u64, ptr: u64, len: u64) -> i64 {
    let len = (len as usize).min(MAX_IO_LEN);
    if fd <= 2 {
        if len > 0 {
            let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
            write_bytes_to_console(bytes);
        }
        return len as i64;
    }
    if len == 0 {
        return 0;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
    match task::write_open_file(fd as usize, bytes, None) {
        Ok(n) => n as i64,
        Err(errno) => errno,
    }
}

/// `writev` -- a scatter/gather `write`, real musl stdio's actual flush
/// path (see the module docs). Each `iovec` is `{ base: u64, len: u64 }`,
/// 16 bytes, matching the real Linux/glibc/musl `struct iovec` layout
/// exactly (this kernel doesn't get to invent that shape -- it's reading
/// memory a real compiled program wrote).
fn sys_writev(_fd: u64, iov_ptr: u64, iovcnt: u64) -> i64 {
    let iovcnt = (iovcnt as usize).min(MAX_IOVEC);
    let mut total = 0i64;
    for i in 0..iovcnt {
        let entry = iov_ptr + (i as u64) * 16;
        let base = unsafe { (entry as *const u64).read_unaligned() };
        let len = unsafe { ((entry + 8) as *const u64).read_unaligned() };
        let len = (len as usize).min(MAX_IO_LEN);
        // A zero-length iovec with a null base is routine (real musl
        // stdio emits exactly this as a trailing/placeholder entry -- see
        // the module docs' strace trace) -- `slice::from_raw_parts`
        // requires a non-null pointer even for a zero-length slice, so
        // this has to be skipped explicitly rather than just falling
        // through with `len == 0`.
        if len > 0 {
            let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, len) };
            write_bytes_to_console(bytes);
        }
        total += len as i64;
    }
    total
}

/// Report the filesystem's existing global CWD; per-process CWD is not yet
/// implemented. Build the owned path before copying so no CWD lock spans a
/// lazy user write. The raw Linux ABI returns length including the NUL.
fn sys_getcwd(buf_ptr: u64, size: u64) -> i64 {
    let path = fat16::cwd_path_string();
    let needed = path.len() + 1;
    if needed > 4096 { return -36; } // ENAMETOOLONG
    if size < needed as u64 { return -34; } // ERANGE
    if buf_ptr == 0 || buf_ptr.checked_add(needed as u64).is_none() { return -14; } // EFAULT
    // Same trusted mapped-user-pointer contract as other copy-out syscalls.
    unsafe {
        core::ptr::copy_nonoverlapping(path.as_ptr(), buf_ptr as *mut u8, path.len());
        ((buf_ptr + path.len() as u64) as *mut u8).write(0);
    }
    needed as i64
}

fn sys_read(fd: u64, buf_ptr: u64, len: u64) -> i64 {
    read_file_chunks(fd, buf_ptr, len, None)
}

/// The staging buffer bounds kernel memory, not the whole regular-file read.
/// libjimage expects a complete class-sized pread when the bytes are available.
/// Keep each disk read and user copy separate so lazy destinations cannot fault
/// under TASKS. Return bytes already copied if a later chunk encounters I/O error.
fn read_file_chunks(fd: u64, buf_ptr: u64, len: u64, offset: Option<u64>) -> i64 {
    let len = len.min(0x7ffff000) as usize; // Linux's maximum single transfer.
    if buf_ptr.checked_add(len as u64).is_none() { return EINVAL; }
    let mut buffer = [0u8; MAX_IO_LEN];
    let mut done = 0usize;
    loop {
        let want = (len - done).min(buffer.len());
        // pread's nonnegative signed offset plus the capped length fits u64.
        let at = offset.map(|start| (start + done as u64) as usize);
        let n = match task::read_open_file(fd as usize, &mut buffer[..want], at) {
            Ok(n) => n,
            Err(errno) => return if done == 0 { errno } else { done as i64 },
        };
        if n != 0 {
            // TASKS is released. Trusted user pointers retain the existing
            // ABI contract; pointer arithmetic was checked before the loop.
            unsafe { core::ptr::copy_nonoverlapping(buffer.as_ptr(), (buf_ptr + done as u64) as *mut u8, n) };
        }
        done += n;
        if n < want || done == len { return done as i64; }
    }
}

/// Real Linux `pread64(2)`: a `read` at an explicit `offset` that leaves
/// `file.pos` (the fd's ordinary sequential position) untouched -- what a
/// real `ld.so` actually uses (found the same ground-truth way as
/// everything else here) to read pieces of an ELF file's header/program
/// table directly by file offset while deciding how to `mmap` it, without
/// disturbing whatever a later plain `read`/`mmap` on the same fd might
/// expect the position to still be.
fn sys_pread64(fd: u64, buf_ptr: u64, len: u64, offset: u64) -> i64 {
    if (offset as i64) < 0 {
        return EINVAL;
    }
    read_file_chunks(fd, buf_ptr, len, Some(offset))
}

/// Regular file seeking. Seeking past EOF is valid; subsequent
/// reads return zero until a position within the file is selected again.
fn sys_lseek(fd: u64, offset: u64, whence: u64) -> i64 {
    task::with_current_open_files(|table| {
        let Some(file) = table.get_mut(fd as usize).and_then(|slot| slot.as_mut()) else {
            return EBADF;
        };
        let real_len = match file.extra.as_deref() {
            Some(task::OpenExtra::Dir(_)) => 0,
            Some(task::OpenExtra::Writable(_, buf)) => buf.len(),
            None => file.data.len(),
        };
        let base = match whence {
            0 => 0, // SEEK_SET
            1 => file.pos, // SEEK_CUR
            2 => real_len, // SEEK_END
            _ => return EINVAL,
        };
        let Ok(base) = i64::try_from(base) else { return EINVAL };
        let Some(next) = base.checked_add(offset as i64) else { return EINVAL };
        if next < 0 {
            return EINVAL;
        }
        // Commit only after validation, so errors preserve the old position.
        file.pos = next as usize;
        next
    })
}

/// Real Linux `close(2)`. A real [`task::OpenExtra::Writable`] fd (see
/// its own doc comment) is flushed back to `fat16` here, best-effort --
/// this filesystem's own write path is already whole-buffer, so "flush
/// on close" is the natural, and only, point a real in-memory write
/// actually reaches disk; a failed flush is silently swallowed rather
/// than turning a real, successful `close(2)` into a surprising error a
/// real caller has no precedent to expect from `close` at all.
fn sys_close(fd: u64) -> i64 {
    let fd = fd as usize;
    let removed = task::with_current_open_files(|table| table.get_mut(fd).and_then(|slot| slot.take()));
    match removed {
        Some(file) => {
            if let Some(task::OpenExtra::Writable(path, buf)) = file.extra.as_deref() {
                let _ = fat16::write_file(path, buf);
            }
            0
        }
        None => EBADF,
    }
}

/// Reads a NUL-terminated string out of ring-3 memory, capped at
/// `MAX_PATH_LEN` -- what a real `open`'s path argument (unlike
/// `syscall.rs`'s own `SYS_OPEN`, which passes an explicit length) always
/// is. Trusted the same limited way as every other pointer argument here.
fn read_c_string(ptr: u64) -> Option<String> {
    // Read one byte at a time, rather than building a fixed-size slice up
    // front -- a real path is usually much shorter than MAX_PATH_LEN, and
    // eagerly reading the full cap could walk off the end of whatever
    // page the string happens to sit near the end of, into an unmapped
    // one (a real fault this kernel would then have to charge to a
    // perfectly ordinary short path string, not to anything the caller
    // did wrong).
    let mut bytes = alloc::vec::Vec::new();
    for i in 0..MAX_PATH_LEN {
        let b = unsafe { ((ptr + i as u64) as *const u8).read() };
        if b == 0 {
            return core::str::from_utf8(&bytes).ok().map(|s| s.to_string());
        }
        bytes.push(b);
    }
    None
}

/// Content for a small, fixed set of real `/proc`/`/sys` pseudo-files,
/// for exactly one real, ground-truthed reason: a real HotSpot JVM's own
/// early CPU-detection code (`os::Linux::...`) reads these during startup
/// to determine how many processors it's running on, and -- unlike
/// glibc's own internal code, which tolerates a missing `/proc`/`/sys`
/// gracefully -- doesn't handle an honest `ENOENT` here safely: it was
/// found, tracing every syscall by hand (this project's own established
/// method), computing some value from the failed lookups and then jumping
/// through it, landing execution on real, unrelated data (see README item
/// 36's own account of chasing this exact crash) rather than real code.
/// This kernel has no real procfs/sysfs -- these are the only paths
/// honored, everything else still honestly falls through to `fat16`/
/// `ENOENT` exactly as before. The content itself is real, not
/// fabricated: this kernel (and this project's own `qemu-system-x86_64`
/// invocation, which never passes `-smp`) only ever runs with one real
/// virtual CPU, so "1 CPU, numbered 0" is what real Linux would honestly
/// report here too, not an invented multi-core fiction.
fn synthetic_proc_file(path: &str) -> Option<&'static [u8]> {
    match path {
        "/proc/stat" => Some(b"cpu  0 0 0 0 0 0 0 0 0 0\ncpu0 0 0 0 0 0 0 0 0 0 0\n"),
        "/sys/devices/system/cpu/possible" | "/sys/devices/system/cpu/online" | "/sys/devices/system/cpu/present" => Some(b"0\n"),
        _ => None,
    }
}

/// Real Linux `sched_getaffinity(2)`: `pid` (a0, unused -- every real
/// caller here asks about itself, and this kernel has no notion of one
/// task's affinity differing from another's anyway) is ignored; `mask` is
/// zeroed for `cpusetsize` bytes (capped, same defensive spirit as every
/// other trusted-pointer write in this module) and bit 0 of byte 0 is set
/// -- real, honest single-CPU affinity, not an invented multi-core one:
/// this kernel (and this project's own `qemu-system-x86_64` invocation,
/// which never passes `-smp`) only ever runs with one real virtual CPU.
/// Found the same ground-truth way as `synthetic_proc_file` right above,
/// hunting the same real HotSpot CPU-count crash (README item 36):
/// real glibc's own `sched_getaffinity` wrapper returns the number of
/// bytes the kernel actually wrote (not the caller's requested
/// `cpusetsize`) for the caller to `popcount`; a real kernel's own
/// internal `cpumask_t` today is at least 8 bytes, so 8 (or less, if the
/// caller's own buffer is smaller) is what's reported back too.
fn sys_sched_getaffinity(cpusetsize: u64, mask_ptr: u64) -> i64 {
    const REAL_CPUMASK_BYTES: u64 = 8;
    let n = cpusetsize.min(REAL_CPUMASK_BYTES);
    if n == 0 {
        return EINVAL;
    }
    unsafe {
        core::ptr::write_bytes(mask_ptr as *mut u8, 0, n as usize);
        (mask_ptr as *mut u8).write(0x01); // CPU 0 only.
    }
    n as i64
}

/// The scheduler runs only on the boot CPU and has no NUMA topology: CPU 0,
/// node 0 agree with sched_getaffinity and the synthetic CPU files. Linux uses
/// unsigned-int outputs, permits either pointer to be NULL, and ignores the
/// obsolete third (cache) argument. No lock may span these lazy user writes.
fn sys_getcpu(cpu_ptr: u64, node_ptr: u64) -> i64 {
    // Safety: user pointers retain this ABI's existing trusted-caller contract.
    // Unaligned stores match Linux's copy-to-user behavior for these u32s.
    unsafe {
        if cpu_ptr != 0 { (cpu_ptr as *mut u32).write_unaligned(0); }
        if node_ptr != 0 { (node_ptr as *mut u32).write_unaligned(0); }
    }
    0
}

/// Real Linux `uname(2)`. Found the exact ground-truth way as everything
/// else in this module: the real `java` binary's own `.note.ABI-tag`
/// requests a minimum kernel version (`file`'s own "for GNU/Linux 3.2.0"
/// -- see README item 39), which real `ld.so` checks by calling `uname`
/// and parsing the `release` field's `X.Y.Z` string with the exact same
/// family of numeric-string parsers this kernel's own JVM bring-up work
/// found crashing (items 36-38) -- unimplemented until now, meaning that
/// check was previously always failing at the `uname` call itself and
/// bailing out immediately, before any string parsing ever ran, which
/// this item's own trace-based investigation already confirmed doesn't
/// match what's actually happening (real numeric parsing genuinely
/// executes before the crash). Reports a real, honest identity for this
/// specific kernel, not a fabricated claim of being real Linux: `sysname`
/// is still `"Linux"` (real glibc code, `ld.so`'s ABI-tag check included,
/// keys behavior on this exact string, and this kernel's whole real-
/// syscall-ABI layer -- see this module's own doc comment -- already
/// exists specifically to be *compatible with* that assumption, not to
/// lie about it for its own sake), `release` is a real, well-formed,
/// modern three-part version (`6.1.0`) chosen to safely satisfy a
/// `3.2.0`-minimum check without being a fabricated *specific* build,
/// and `machine`/`version` honestly name this project. Every field is a
/// fixed 65-byte, NUL-padded buffer (`struct new_utsname`'s real
/// on-disk/ABI layout -- six of them, `390` bytes total), same as a real
/// kernel's own `sys_uname` writes.
fn sys_uname(buf_ptr: u64) -> i64 {
    const FIELD_LEN: usize = 65;
    let fields: [&[u8]; 6] = [b"Linux", b"konjacos", b"6.1.0", b"#1 KonjacOS", b"x86_64", b"(none)"];
    for (i, field) in fields.iter().enumerate() {
        let mut buf = [0u8; FIELD_LEN];
        buf[..field.len()].copy_from_slice(field);
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), (buf_ptr + (i * FIELD_LEN) as u64) as *mut u8, FIELD_LEN);
        }
    }
    0
}

/// Real Linux `sysinfo(2)`. Found the exact ground-truth way as everything
/// else in this module: a real HotSpot JVM calls this during its own
/// early startup (ergonomics -- sizing its default heap off *real*
/// available memory, not a guess), and this kernel had nothing to answer
/// it with at all before now. `uptime`/`totalram`/`freeram` are real,
/// live values -- `timer::uptime_seconds()` and `pmm::stats()` (already
/// this kernel's own honest source of truth for `meminfo`), not
/// fabricated numbers -- while `sharedram`/`bufferram`/`totalswap`/
/// `freeswap`/`procs`/`totalhigh`/`freehigh` are honestly zeroed: this
/// kernel has no shared memory, no page cache, no swap, and no real
/// process-count accounting exposed anywhere else either, so zero is the
/// honest answer, not a plausible-looking invented one. `mem_unit` is `1`
/// (byte-granular, matching `totalram`/`freeram` already being reported
/// in bytes, not pages) -- real Linux sets this to something other than 1
/// only when a memory value would otherwise overflow the 32-bit field on
/// a 32-bit kernel, never a concern here. Real `struct sysinfo`'s exact
/// x86_64 layout (`(u)long uptime`/`loads[3]`/`totalram`/`freeram`/
/// `sharedram`/`bufferram`/`totalswap`/`freeswap`, `u16 procs`+`u16 pad`,
/// `(u)long totalhigh`/`freehigh`, `u32 mem_unit`), zero-filled first so
/// any trailing padding a real libc might read is honestly zero too.
fn sys_sysinfo(info_ptr: u64) -> i64 {
    const SYSINFO_SIZE: usize = 112;
    let mut buf = [0u8; SYSINFO_SIZE];
    let (total_frames, free_frames) = pmm::stats();
    let uptime = crate::timer::uptime_seconds() as i64;
    buf[0..8].copy_from_slice(&uptime.to_le_bytes());
    buf[32..40].copy_from_slice(&(total_frames * PAGE_SIZE).to_le_bytes()); // totalram
    buf[40..48].copy_from_slice(&(free_frames * PAGE_SIZE).to_le_bytes()); // freeram
    // x86_64 pads after procs/pad: totalhigh is at 88, freehigh at 96,
    // and mem_unit at 104. Writing at 100 corrupts freehigh and leaves
    // libc reporting zero bytes of physical memory to JVM ergonomics.
    buf[104..108].copy_from_slice(&1u32.to_le_bytes()); // mem_unit
    unsafe {
        core::ptr::copy_nonoverlapping(buf.as_ptr(), info_ptr as *mut u8, SYSINFO_SIZE);
    }
    0
}

/// Real Linux `open(2)`. Originally used for exactly one thing: musl's
/// own `dlopen`, needing an arbitrary named file at runtime rather than
/// whatever `PT_INTERP`/`DT_NEEDED` mapped up front (see `loader.rs`'s
/// module docs and item 23's README entry) -- every open was read-only in
/// practice then, so `flags`/`mode` went unconsulted.
///
/// Real HotSpot's own `java -version` startup (see `docs/java-version.md`)
/// needed more: opening an existing *directory* (`O_DIRECTORY`, for its
/// own `fchdir` dance -- see `sys_fchdir`) and creating/writing a real
/// regular file (`O_CREAT`, for its `hsperfdata` PerfData region). Both
/// now real:
///
/// * A path that's already a directory opens as [`task::OpenExtra::Dir`]
///   -- `data` stays an inert placeholder, real content lives in `extra`
///   (see its own doc comment for why, and `sys_fstat`/`sys_read` for how
///   each caller tells the two apart).
/// * A path opened with a write mode (`O_WRONLY`/`O_RDWR`, i.e.
///   `flags & O_ACCMODE != 0`) -- whether it already exists or is being
///   created fresh (`O_CREAT`, and only then) -- opens as a real
///   [`task::OpenExtra::Writable`], its full content read into memory
///   up front (empty for a brand new file) and flushed back to `fat16`
///   on close (`sys_close`).
/// * A plain read-only open of a regular file keeps the exact previous
///   path unchanged (`FileBacking::Disk`, real lazy/bounded reads --
///   see items 46/49) -- nothing about this addition touches the common
///   case a real `libjimage`/library load already depends on.
///
/// `mode` (the real third argument) is still read but not consulted --
/// same reasoning as ever: no real permission model exists here for it to
/// mean anything against.
fn sys_open(path_ptr: u64, flags: u64, _mode: u64) -> i64 {
    let Some(path) = read_c_string(path_ptr) else {
        return EINVAL;
    };
    let ino = task::hash_path(&path);
    let write_mode = flags & O_ACCMODE != 0;
    let want_dir = flags & O_DIRECTORY != 0;

    let opened: Result<(FileBacking, Option<task::OpenExtra>), i64> = if let Some(content) = synthetic_proc_file(&path) {
        Ok((FileBacking::Static(content), None))
    } else {
        match fat16::stat_path(&path) {
            Ok((true, _)) if write_mode => Err(EISDIR),
            Ok((true, _)) => Ok((FileBacking::Static(&[]), Some(task::OpenExtra::Dir(path.clone())))),
            Ok((false, _)) if want_dir => Err(ENOTDIR),
            Ok((false, _)) if write_mode => match fat16::read_file(&path) {
                Ok(bytes) => Ok((FileBacking::Static(&[]), Some(task::OpenExtra::Writable(path.clone(), bytes)))),
                Err(_) => Err(ENOENT),
            },
            Ok((false, _)) => fat16::open_file(&path).map(|f| (FileBacking::Disk(f), None)).map_err(|_| ENOENT),
            Err(_) if flags & O_CREAT != 0 => Ok((FileBacking::Static(&[]), Some(task::OpenExtra::Writable(path.clone(), Vec::new())))),
            Err(_) => Err(ENOENT),
        }
    };

    match opened {
        Ok((data, extra)) => task::with_current_open_files(|table| match table.iter().position(|f| f.is_none()) {
            Some(fd) => {
                table[fd] = Some(OpenFile { data, pos: 0, ino, extra: extra.map(alloc::boxed::Box::new) });
                fd as i64
            }
            None => EBADF, // every fd slot is in use -- a real ENFILE/EMFILE would be more accurate, but EBADF is already this module's established "ran out of fd-table-shaped resources" answer (see sys_close).
        }),
        Err(errno) => errno,
    }
}

/// Real Linux `mkdir(2)`. `mode` (the second argument) is read but not
/// consulted -- same reasoning `sys_open`'s own flags/mode doc comment
/// already gives: this kernel has no real permission model for a mode
/// bit to mean anything against (see `sys_prlimit64`/uid-syscalls' own
/// doc comments for the same point made elsewhere in this module).
/// Onto `fat16::create_dir` -- see its own doc comment for the real,
/// ground-truthed reason this exists: a real `java -version` calls this
/// for its own `/tmp/hsperfdata_<user>` PerfData directory.
fn sys_mkdir(path_ptr: u64) -> i64 {
    let Some(path) = read_c_string(path_ptr) else {
        return EINVAL;
    };
    match fat16::create_dir(&path) {
        Ok(()) => 0,
        Err("already exists") => EEXIST,
        Err(_) => ENOENT,
    }
}

/// Real Linux `unlink(2)`, onto `fat16::remove_file`. Real HotSpot calls
/// this at real shutdown, on its own `hsperfdata` PerfData file (see
/// `docs/java-version.md`) -- observed for the first time only once the
/// rest of that file's own real creation dance (`mkdir`/`fchdir`/
/// `ftruncate`/...) started succeeding rather than being silently
/// abandoned partway through.
fn sys_unlink(path_ptr: u64) -> i64 {
    let Some(path) = read_c_string(path_ptr) else {
        return EINVAL;
    };
    match fat16::remove_file(&path) {
        Ok(()) => 0,
        Err(_) => ENOENT,
    }
}

/// Real Linux `fchdir(2)`. Onto `task::open_file_dir_path`/
/// `fat16::change_dir` -- see `task::OpenFile::extra`'s own doc comment
/// for why this needs an open directory fd's own resolved path rather
/// than a new, real per-task-cwd concept: this kernel's `fat16::CWD` was
/// already a single global before this (see item 49's own "this does not
/// add per-process CWD" callout), and reusing it here is the same
/// honestly-narrower-than-real-Linux scope, not a new limitation.
fn sys_fchdir(fd: u64) -> i64 {
    match task::open_file_dir_path(fd as usize) {
        Some(path) => match fat16::change_dir(&path) {
            Ok(()) => 0,
            Err(_) => ENOENT,
        },
        None => EBADF,
    }
}

/// Real Linux `ftruncate(2)`, onto `task::truncate_open_file` -- see its
/// own doc comment.
fn sys_ftruncate(fd: u64, size: u64) -> i64 {
    match task::truncate_open_file(fd as usize, size) {
        Ok(()) => 0,
        Err(errno) => errno,
    }
}

/// Real Linux `fstat(2)`, filling in only what a real caller here has
/// ever actually needed (found the same ground-truth way as everything
/// else in this module): `st_mode` (just enough to say "a regular file,
/// mode 0644" -- `dlopen`'s own internal bookkeeping checks `S_ISREG`)
/// and `st_size` (musl's `dlopen` uses this to size its `PROT_READ`
/// `mmap` of the whole file before mapping individual `PT_LOAD` segments
/// over parts of it -- see `sys_mmap`'s file-backed case). Everything
/// else in the 144-byte real `struct stat` (see `STAT_SIZE`) -- device/
/// inode numbers, link count, timestamps, block counts -- is honestly
/// zeroed rather than fabricated, since nothing that's actually run
/// against this kernel so far has needed any of it to be real.
fn sys_fstat(fd: u64, statbuf_ptr: u64) -> i64 {
    let Some((size, ino, is_dir)) = task::open_file_len(fd as usize) else {
        return EBADF;
    };
    write_stat(statbuf_ptr, size, ino, is_dir);
    0
}

/// The actual `struct stat`-filling logic behind both `sys_fstat` (an
/// already-open fd) and `sys_newfstatat` (a bare path) -- identical either
/// way once `size`/`ino`/`is_dir` are known, so it's factored out rather
/// than duplicated. `is_dir` matters for real reasons beyond just
/// `S_ISDIR` looking right cosmetically: real glibc `ld.so`'s own RPATH
/// directory-existence caching (`_dl_map_object`'s `open_path`) calls
/// `fstatat` on a search *directory* itself before trusting it, and a
/// directory reported as anything other than `S_IFDIR` reads as "this
/// isn't really a directory" -- see `sys_newfstatat`'s doc comment for the
/// real failure this caused before `fat16::stat_path` could report one.
fn write_stat(statbuf_ptr: u64, size: u64, ino: u64, is_dir: bool) {
    // st_dev: a constant 1 -- there's only ever one filesystem here (see
    // fat16.rs), so any single nonzero value is honest and consistent.
    // st_ino: see OpenFile::ino's doc comment for why this can't just be
    // 0 like every other field this kernel doesn't have real data for --
    // real musl dlopen's own already-loaded-object de-duplication
    // depends on (st_dev, st_ino) actually being distinct per file.
    const S_IFREG: u32 = 0o100000;
    const S_IFDIR: u32 = 0o040000;
    let mode = if is_dir { S_IFDIR | 0o755 } else { S_IFREG | 0o644 };
    let mut buf = [0u8; STAT_SIZE];
    buf[0..8].copy_from_slice(&1u64.to_le_bytes()); // st_dev
    buf[8..16].copy_from_slice(&ino.to_le_bytes()); // st_ino
    buf[16..24].copy_from_slice(&1u64.to_le_bytes()); // st_nlink
    buf[24..28].copy_from_slice(&mode.to_le_bytes()); // st_mode
    buf[48..56].copy_from_slice(&size.to_le_bytes()); // st_size
    unsafe {
        core::ptr::copy_nonoverlapping(buf.as_ptr(), statbuf_ptr as *mut u8, STAT_SIZE);
    }
}

/// Real Linux `newfstatat(2)` (a.k.a. `fstatat`) -- what a real glibc
/// `ld.so` actually calls instead of plain `stat`/`open` while walking its
/// library search path, found the same ground-truth way as everything
/// else in this module: a real `ld-linux-x86-64.so.2` booted under this
/// kernel (see README item 29) called this, repeatedly, once its own
/// relocations stopped being the blocker. `dirfd` (the real first
/// argument) is read but not consulted, same as `sys_openat` below and for
/// the same reason -- every path this kernel has ever seen a real loader
/// ask about has been absolute, and `fat16::read_file` already treats a
/// path as rooted regardless of any notion of "current directory" a
/// `dirfd` could otherwise redirect (see its own doc comment) -- so a
/// `dirfd` other than `AT_FDCWD` would silently behave identically to
/// `AT_FDCWD` here rather than actually erroring on the rare real program
/// that relies on it meaning something else. `flags` (e.g. `AT_EMPTY_PATH`,
/// `AT_SYMLINK_NOFOLLOW`) is likewise unconsulted -- there are no symlinks
/// and no empty-path-means-dirfd-itself case to honor on a flat FAT16
/// volume with no such concept at all.
fn sys_newfstatat(path_ptr: u64, statbuf_ptr: u64) -> i64 {
    let Some(path) = read_c_string(path_ptr) else {
        return EINVAL;
    };
    if let Some(content) = synthetic_proc_file(&path) {
        write_stat(statbuf_ptr, content.len() as u64, task::hash_path(&path), false);
        return 0;
    }
    match fat16::stat_path(&path) {
        Ok((is_dir, size)) => {
            write_stat(statbuf_ptr, size as u64, task::hash_path(&path), is_dir);
            0
        }
        Err(_) => ENOENT,
    }
}

/// Real Linux `access(2)`: `mode` (`F_OK`/`R_OK`/`W_OK`/`X_OK`) is read but
/// not consulted -- this filesystem is read-only in practice (see
/// `fat16.rs`) and has no execute-permission concept at all, so the only
/// honest question this can actually answer is "does this exist" (`F_OK`),
/// which is also the only one every real caller seen here so far has
/// needed. Backed by [`fat16::stat_path`], the same directory-or-file
/// lookup `sys_newfstatat` uses -- a real `ld.so`/`libjli` genuinely
/// probes both files (`access(candidate_jrepath, F_OK)`-style checks
/// while hunting for a JRE layout) and directories this way.
fn sys_access(path_ptr: u64) -> i64 {
    let Some(path) = read_c_string(path_ptr) else {
        return EINVAL;
    };
    if synthetic_proc_file(&path).is_some() {
        return 0;
    }
    match fat16::stat_path(&path) {
        Ok(_) => 0,
        Err(_) => ENOENT,
    }
}

/// Real Linux `readlink(2)`, honestly narrow: there's no real filesystem
/// symlink support here at all (`fat16.rs` has no notion of one), and no
/// procfs either -- but real glibc `ld.so` and `libjli` both genuinely
/// depend on exactly one specific "symlink" existing, `/proc/self/exe`,
/// to find their own real invocation path (`ld.so` uses it to expand a
/// `$ORIGIN` in its own `RPATH`; `libjli`'s `GetJREPath` uses it as its
/// *first* strategy, before ever falling back to `argv[0]` -- see item 33's
/// README entry for that fallback path failing without this). Real Linux
/// backs `/proc/self/exe` with the kernel's own record of which file
/// `execve` actually ran, not a disk-backed symlink either, so special-
/// casing exactly this one path and answering it from `task::exe_path`
/// implements the executable link without a general procfs. Ordinary
/// FAT16 paths are checked for existence: existing non-links return
/// `EINVAL`, missing paths return `ENOENT`. Returns the byte count
/// written into `buf`, truncated to `bufsize`, with no NUL terminator
/// appended (a real symlink's target isn't NUL-terminated in the syscall's
/// own returned bytes either -- the caller's `bufsize` is what bounds it).
fn sys_readlink(path_ptr: u64, buf_ptr: u64, bufsize: u64) -> i64 {
    if bufsize == 0 {
        return EINVAL;
    }
    let Some(path) = read_c_string(path_ptr) else {
        return EINVAL;
    };
    if path.is_empty() {
        return ENOENT;
    }
    if path != "/proc/self/exe" {
        // FAT16 has no symlinks, but an existing non-link is EINVAL, not
        // ENOENT. glibc realpath probes each path component with readlink:
        // EINVAL means keep walking; ENOENT means the path does not exist.
        return match fat16::stat_path(&path) {
            Ok(_) => EINVAL,
            Err(_) => ENOENT,
        };
    }
    let exe = task::current_exe_path();
    if exe.is_empty() {
        return ENOENT; // Honest: nothing to report for a task that wasn't spawned from a real file path (see current_exe_path's doc comment).
    }
    let bytes = exe.as_bytes();
    let n = bytes.len().min(bufsize as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr as *mut u8, n);
    }
    n as i64
}

/// Real Linux `clock_gettime(2)`. Honest about the one real limitation:
/// there's no RTC driver here (see this module's own search for one --
/// there isn't -- and `timer.rs`'s module docs, which only ever promised
/// an uptime counter), so there's no way to answer `CLOCK_REALTIME` with
/// genuine wall-clock/UTC time at all. Rather than fabricate a plausible-
/// looking timestamp (which `glibc`/a JVM could easily use for something
/// that matters, e.g. file timestamps or log lines, silently wrong),
/// `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, and `CLOCK_BOOTTIME` all honestly
/// answer with the same real, ground-truthed value: seconds/nanoseconds
/// derived from `timer::ticks()`, i.e. genuine elapsed time *since this
/// kernel booted* -- correct for `CLOCK_MONOTONIC`/`CLOCK_BOOTTIME`'s own
/// actual contract (neither one promises to relate to wall-clock time at
/// all), and honestly wrong-but-labeled for `CLOCK_REALTIME` in exactly
/// the way this module already prefers over silent fabrication elsewhere
/// (see `sys_fstat`'s zeroed timestamp fields). Any other clock ID (CPU-
/// time clocks, `CLOCK_MONOTONIC_RAW`, ...) isn't backed by anything real
/// here and is honestly rejected with `EINVAL` instead.
fn sys_clock_gettime(clk_id: u64, ts_ptr: u64) -> i64 {
    if clk_id != CLOCK_REALTIME && clk_id != CLOCK_MONOTONIC && clk_id != CLOCK_BOOTTIME {
        return EINVAL;
    }
    let ticks = timer::ticks();
    let tv_sec = (ticks / timer::HZ as u64) as i64;
    let tv_nsec = ((ticks % timer::HZ as u64) * (1_000_000_000 / timer::HZ as u64)) as i64;
    unsafe {
        core::ptr::write_unaligned(ts_ptr as *mut i64, tv_sec);
        core::ptr::write_unaligned((ts_ptr + 8) as *mut i64, tv_nsec);
    }
    0
}

/// Real Linux `gettimeofday(2)` -- superseded by `clock_gettime` on
/// modern glibc for anything that actually cares about a specific clock,
/// but still called directly during real HotSpot startup (observed:
/// before `java -version`'s own banner ever prints). Backed by the same
/// boot-relative `timer::ticks()` source `sys_clock_gettime` already
/// uses, just reported as `{sec, usec}` (a real `struct timeval`) instead
/// of `{sec, nsec}`. `tz_ptr`, the second, long-obsolete `struct
/// timezone` argument, is real Linux's own permanent no-op -- ignored
/// here for the same reason real glibc's own wrapper never fills it in
/// either.
fn sys_gettimeofday(tv_ptr: u64, _tz_ptr: u64) -> i64 {
    if tv_ptr == 0 {
        return 0;
    }
    let ticks = timer::ticks();
    let tv_sec = (ticks / timer::HZ as u64) as i64;
    let tv_usec = ((ticks % timer::HZ as u64) * (1_000_000 / timer::HZ as u64)) as i64;
    unsafe {
        core::ptr::write_unaligned(tv_ptr as *mut i64, tv_sec);
        core::ptr::write_unaligned((tv_ptr + 8) as *mut i64, tv_usec);
    }
    0
}

/// Real Linux `clock_getres(2)`. Reports this kernel's own real,
/// honest timer granularity -- one PIT tick (`timer::HZ`, currently
/// 100Hz, i.e. 10 milliseconds) -- rather than a fabricated
/// "1 nanosecond" a genuine high-resolution clock source would claim:
/// every clock id this kernel answers (`sys_clock_gettime`'s own
/// REALTIME/MONOTONIC/BOOTTIME) is backed by that same PIT tick counter,
/// so they honestly share one real resolution.
fn sys_clock_getres(clk_id: u64, res_ptr: u64) -> i64 {
    if clk_id != CLOCK_REALTIME && clk_id != CLOCK_MONOTONIC && clk_id != CLOCK_BOOTTIME {
        return EINVAL;
    }
    if res_ptr != 0 {
        let nsec = 1_000_000_000 / timer::HZ as i64;
        unsafe {
            core::ptr::write_unaligned(res_ptr as *mut i64, 0i64);
            core::ptr::write_unaligned((res_ptr + 8) as *mut i64, nsec);
        }
    }
    0
}

/// Real Linux `prctl(2)`. Observed so far only from real glibc's own
/// per-thread startup (`PR_SET_NAME`, naming the new thread for `/proc`/
/// debugger display) -- accepted as a no-op for every option, the same
/// "real syscall, no observable effect this kernel's own `ps` needs yet"
/// treatment `SYS_FCNTL` already gets above. A specific option feeding
/// into `ps`'s own display can grow real behavior here later; nothing
/// observed calling this so far depends on it doing more than
/// succeeding.
fn sys_prctl(_option: u64) -> i64 {
    0
}

/// Real Linux `prlimit64(2)`, called by real glibc/HotSpot startup to
/// query resource limits (`RLIMIT_STACK` sizes JVM guard pages;
/// `RLIMIT_NOFILE` sizes descriptor-table ergonomics). The first argument
/// (`pid`) is always this task's own real pid in every call observed, so
/// it's read but not consulted here, same treatment `SYS_OPENAT`'s
/// `dirfd` already gets. *Setting* a new limit (`new_limit != 0`) is
/// honestly refused with `ENOSYS`: this kernel enforces no actual
/// resource limits for a new one to change the meaning of. *Querying*
/// (`old_limit != 0`) answers with real, sane values -- `RLIMIT_NOFILE`
/// matches this kernel's own fixed per-task open-file table size (see
/// `task::MAX_OPEN_FILES`), `RLIMIT_STACK` matches real Linux's own
/// common default (8 MiB soft, unlimited hard); every other resource
/// honestly reports unlimited rather than a fabricated specific number
/// this kernel doesn't actually track or enforce.
fn sys_prlimit64(resource: u64, new_limit: u64, old_limit: u64) -> i64 {
    if new_limit != 0 {
        return ENOSYS;
    }
    if old_limit != 0 {
        let (cur, max): (u64, u64) = match resource {
            RLIMIT_NOFILE => (task::MAX_OPEN_FILES as u64, task::MAX_OPEN_FILES as u64),
            RLIMIT_STACK => (8 * 1024 * 1024, RLIM_INFINITY),
            _ => (RLIM_INFINITY, RLIM_INFINITY),
        };
        unsafe {
            core::ptr::write_unaligned(old_limit as *mut u64, cur);
            core::ptr::write_unaligned((old_limit + 8) as *mut u64, max);
        }
    }
    0
}

/// Real Linux `getrandom(2)`. `flags` (`GRND_NONBLOCK`/`GRND_RANDOM`) is
/// read but not consulted -- there's no blocking entropy pool here to
/// distinguish them from. Backed by real hardware randomness only: this
/// checks `CPUID.1:ECX[30]` (the same "verify, don't guess" discipline
/// `paging::init`'s own `CPUID` check for NX support already established
/// -- see item 27) for `RDRAND` support before ever using it, and honestly
/// refuses with `ENOSYS` rather than fabricate "random" bytes from
/// something predictable (`timer::ticks()`, an address, ...) if the CPU
/// (or, in practice, whichever QEMU `-cpu` model this is running under --
/// plain `qemu64` TCG does not expose it) doesn't actually support it.
/// Real randomness or an honest refusal, never a fake CSPRNG pretending to
/// be one.
fn sys_getrandom(buf_ptr: u64, len: u64) -> i64 {
    if !rdrand_available() {
        return ENOSYS;
    }
    let len = (len as usize).min(MAX_IO_LEN);
    let mut written = 0usize;
    while written < len {
        let Some(word) = rdrand64() else {
            break; // Real hardware transient failure (exhausted its own internal entropy conditioner) -- Intel's own guidance is to retry a bounded number of times, already done inside rdrand64; giving up beyond that and returning whatever's been filled so far is honest (a real short read), not wrong.
        };
        let bytes = word.to_le_bytes();
        let n = (len - written).min(8);
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), (buf_ptr + written as u64) as *mut u8, n);
        }
        written += n;
    }
    if written == 0 { EAGAIN } else { written as i64 }
}

/// `CPUID.1:ECX[30]` -- real hardware `RDRAND` support, checked fresh each
/// call rather than cached: cheap enough (one `CPUID` leaf-1 query) that
/// caching it in a static, `paging::nx_available`-style, would only add
/// state for no measurable benefit.
fn rdrand_available() -> bool {
    const RDRAND_ECX_BIT: u32 = 1 << 30;
    let ecx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "pop rbx",
            inout("eax") 1u32 => _,
            out("ecx") ecx,
            out("edx") _,
            options(nostack, preserves_flags),
        );
    }
    ecx & RDRAND_ECX_BIT != 0
}

/// One real `rdrand` draw, retried up to 10 times on transient failure
/// (`CF=0`) -- Intel's own documented guidance (the DRNG conditioner can
/// briefly run dry under heavy demand; a handful of retries is expected,
/// normal operation, not a sign anything's wrong), returning `None` only
/// if it still hasn't succeeded after that many tries.
fn rdrand64() -> Option<u64> {
    for _ in 0..10 {
        let value: u64;
        let ok: u8;
        // Deliberately no `preserves_flags`: `rdrand` itself sets CF to
        // signal success/failure, which `setc` then actually reads --
        // claiming flags survive this block untouched would be a lie the
        // compiler could act on.
        unsafe {
            core::arch::asm!(
                "rdrand {val}",
                "setc {ok}",
                val = out(reg) value,
                ok = out(reg_byte) ok,
                options(nostack),
            );
        }
        if ok != 0 {
            return Some(value);
        }
    }
    None
}

/// Identical semantics to `syscall.rs::SYS_BRK` -- see its doc comment --
/// just reached through the real `brk(2)` number instead of KonjacOS's
/// own.
fn sys_brk(requested: u64) -> u64 {
    let current = task::heap_end();
    if requested == 0 || requested <= current {
        return current;
    }
    let target = requested.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let mut virt = current;
    while virt < target {
        let Some(phys) = pmm::alloc_frame() else { break };
        unsafe {
            paging::map_page(virt, phys, paging::PAGE_WRITABLE | paging::PAGE_USER);
            core::ptr::write_bytes((pmm::hhdm_offset() + phys) as *mut u8, 0, PAGE_SIZE as usize);
        }
        virt += PAGE_SIZE;
    }
    task::set_heap_end(virt);
    virt
}

/// Linux mmap delegates reservations and backing to the shared-address-space
/// mapping table. The assembly dispatch has five arguments; offset is saved r9.
fn sys_mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64) -> i64 {
    // Safety: this is the current live syscall frame, unchanged by this call.
    let offset = unsafe { ((gdt::SYSCALL_KERNEL_RSP - 128 + 5 * 8) as *const u64).read() };
    if flags & MAP_ANONYMOUS == 0 {
        let writable = task::with_current_open_files(|table| {
            let file = table.get(fd as usize).and_then(|f| f.as_ref())?;
            match file.extra.as_deref() {
                Some(task::OpenExtra::Writable(_, buf)) => Some(buf.clone()),
                _ => None,
            }
        });
        if let Some(bytes) = writable {
            return sys_mmap_writable_fd(addr, len, prot, flags, offset, &bytes);
        }
        let backing = match task::with_current_open_files(|table| table.get(fd as usize).and_then(|f| f.as_ref()).map(|f| f.data)) {
            Some(data) => data,
            None => return EBADF,
        };
        return crate::vm::map(addr, len, prot, flags, Some(backing), offset).unwrap_or_else(|errno| errno);
    }
    crate::vm::map(addr, len, prot, flags, None, offset).unwrap_or_else(|errno| errno)
}

/// Real `MAP_SHARED` + writable file-backed `mmap` onto a real
/// [`task::OpenExtra::Writable`] fd -- what HotSpot's own `hsperfdata`
/// PerfData region needs (see `docs/java-version.md`). `vm.rs`'s own
/// general file-backed mapping path explicitly refuses this exact
/// combination (`prot & 2 != 0` with `MAP_SHARED`, "shared writable pages
/// require cache/writeback semantics we do not have") -- correctly, for
/// the general case this kernel has no real page-cache/writeback path
/// for. But there's never a second process (or even a second reader) for
/// this kernel's own writable fds to actually share memory *with* -- so
/// this narrower path handles the one real caller found so far honestly:
/// reserve an ordinary anonymous region through the existing `vm::map`
/// (real address allocation, capacity/region bookkeeping, and process-
/// teardown frame cleanup all unchanged and untouched), eagerly populate
/// every page through the exact same `vm::fault` a lazy first access
/// would have taken (eagerly rather than lazily, since the content needs
/// to be right *before* this call returns, not on first touch), then
/// copy the fd's current buffer content in. Not a real shared mapping in
/// the cross-process sense -- an honest, narrower stand-in for the one
/// real thing that actually calls this.
fn sys_mmap_writable_fd(addr: u64, len: u64, prot: u64, flags: u64, offset: u64, bytes: &[u8]) -> i64 {
    let anon_flags = (flags & !3) | MAP_ANONYMOUS | 2; // force MAP_PRIVATE|MAP_ANONYMOUS, keep MAP_FIXED and any other bits.
    let start = match crate::vm::map(addr, len, prot, anon_flags, None, offset) {
        Ok(start) => start as u64,
        Err(errno) => return errno,
    };
    let page_count = len.div_ceil(PAGE_SIZE);
    for i in 0..page_count {
        if !crate::vm::fault(start + i * PAGE_SIZE, 2) {
            return ENOMEM;
        }
    }
    let copy_len = bytes.len().min(len as usize);
    if copy_len > 0 {
        // Safety: every page in [start, start+len) was just eagerly
        // populated and mapped PAGE_USER|PAGE_WRITABLE above, in this
        // exact address space (this syscall runs with the caller's own
        // CR3 already active) -- a plain kernel-context write through
        // the user VA is exactly what every other in-place write in this
        // module already does to freshly mapped user memory.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), start as *mut u8, copy_len);
        }
    }
    start as i64
}

fn sys_munmap(addr: u64, len: u64) -> i64 {
    crate::vm::unmap(addr, len).map(|_| 0).unwrap_or_else(|errno| errno)
}

fn sys_mprotect(addr: u64, len: u64, prot: u64) -> i64 {
    crate::vm::protect(addr, len, prot).map(|_| 0).unwrap_or_else(|errno| errno)
}

/// Real Linux `clone(2)`, for real musl `pthread_create` -- the second
/// half of the "resuming mid-syscall-epilogue" architecture described in
/// the module docs and implemented by `task::spawn_clone_raw`. Unlike
/// `syscall.rs`'s own, much simpler `SYS_CLONE` (which just starts a
/// brand new task at a caller-given entry point -- KonjacOS's own ABI
/// invented that convention itself, so it's free to), a *real* `clone()`
/// has no entry point argument at all: real Linux/musl/glibc code expects
/// the child to come back from the exact same `syscall` instruction the
/// parent did, on the stack it was given, with `rax=0` -- and picks up
/// running entirely on its own from there. Musl's own `__clone` assembly
/// stub (already sitting on that stack, pushed there by `pthread_create`
/// before ever calling this) is what actually calls the thread's start
/// function and then `exit`s -- this kernel doesn't need to know or care
/// what runs after the child resumes, only that it resumes *correctly*.
///
/// Real x86_64 raw syscall argument order for `clone` -- ground-truthed
/// the same way as everything else in this module, against the actual
/// Linux `arch/x86/` calling convention, not a guess: `flags` (rdi/a0),
/// `child_stack` (rsi/a1), `parent_tidptr` (rdx/a2), `child_tidptr`
/// (r10/a3), `tls` (r8/a4) -- note `child_tidptr`/`tls` swap places
/// relative to the C-level `clone(2)` man page prototype, a well-known
/// x86_64 quirk.
///
/// Implements the three flags real musl `pthread_create` actually sets
/// (found via this module's own `strace` ground-truthing against a real
/// `pthreadtest.c`): `CLONE_SETTLS` (the new thread's own TLS block,
/// distinct from the parent's -- without this, every field musl's
/// per-thread struct keeps at `%fs:`-relative offsets, including the
/// child's own stack-protector canary and its own `tid`, would alias the
/// parent's), `CLONE_PARENT_SETTID` (write the new tid back into the
/// parent's `pthread_t` synchronously, before this syscall even returns
/// to the parent -- so the parent can never observe its own
/// `pthread_create` returning before that tid is valid), and
/// `CLONE_CHILD_CLEARTID` (handled at child exit time, not here -- see
/// `task::task_exit`'s doc comment). CLONE_FILES is required and retains the
/// parent's descriptor table. The existing thread-only path shares CR3, but
/// does not fully implement the remaining CLONE_FS/SIGHAND/THREAD/SYSVSEM
/// contracts; notably signal actions and brk still have per-task state.
fn sys_clone(flags: u64, child_stack: u64, parent_tidptr: u64, child_tidptr: u64, tls: u64) -> i64 {
    if child_stack == 0 {
        // Real Linux allows a NULL stack only for a plain fork()-like
        // clone (no CLONE_VM); every actual caller here is thread
        // creation, which always provides one -- an honest refusal
        // instead of silently running the child on the parent's own
        // stack.
        return EINVAL;
    }
    do_clone(flags, child_stack, parent_tidptr, child_tidptr, tls)
}

/// The actual "resume mid-syscall-epilogue" logic behind both real clone
/// entry points -- legacy `clone(2)` ([`sys_clone`]) and modern `clone3(2)`
/// ([`sys_clone3`]) -- identical from here on regardless of which one the
/// caller used to get here: both ultimately reduce to "these flags, this
/// *already-the-top* child stack pointer, these two tid pointers, this TLS
/// base." `child_stack_top` is named that deliberately, not `child_stack`
/// -- see `sys_clone3`'s own doc comment for the real, easy-to-get-wrong
/// reason the two callers computes aren't the same expression.
fn do_clone(flags: u64, child_stack_top: u64, parent_tidptr: u64, child_tidptr: u64, tls: u64) -> i64 {
    // This thread-only clone path shares file tables. A clone without FILES
    // needs descriptor-table copying with shared open-file descriptions;
    // refuse it explicitly rather than silently giving an empty/shared table.
    if flags & CLONE_FILES == 0 { return ENOSYS; }
    // Snapshot the parent's own live syscall frame. `gdt::SYSCALL_KERNEL_RSP`
    // is this task's `kernel_stack_top` (see `gdt::set_kernel_stack`/
    // `task::schedule`, which keep it current on every switch);
    // `linux_syscall_entry` always pushes exactly 16 u64s (128 bytes) onto
    // it, in the fixed order documented on `task::spawn_clone_raw`, before
    // ever calling into `linux_syscall_handler` -- so this address is
    // always exactly where that live, in-progress frame currently sits,
    // still on *this* task's own kernel stack (we haven't switched away
    // from it yet).
    let base = unsafe { gdt::SYSCALL_KERNEL_RSP } - 128;
    let mut frame = [0u64; 16];
    for (i, word) in frame.iter_mut().enumerate() {
        *word = unsafe { ((base + (i as u64) * 8) as *const u64).read() };
    }
    // Index 12 = rax -- held the clone syscall number (56) in the parent's
    // own frame; the child's copy of that slot becomes its return value
    // from clone(), which real Linux defines as 0.
    frame[12] = 0;
    // Index 15 = the saved user RSP -- the child resumes on its own given
    // stack, not the parent's.
    frame[15] = child_stack_top;

    let fs_base = if flags & CLONE_SETTLS != 0 { tls } else { task::fs_base() };
    let child_tidptr_opt = if flags & CLONE_CHILD_CLEARTID != 0 { Some(child_tidptr) } else { None };

    unsafe extern "C" {
        fn linux_syscall_resume_frame();
    }
    let resume_addr = linux_syscall_resume_frame as *const () as u64;

    let cr3 = paging::current_cr3();
    let heap_end = task::heap_end();

    let Some(child_id) = task::spawn_clone_raw("thread", resume_addr, frame, cr3, heap_end, fs_base, child_tidptr_opt, task::current_exe_path()) else {
        return EAGAIN;
    };

    if flags & CLONE_PARENT_SETTID != 0 && parent_tidptr != 0 {
        // Trusted pointer, same as every other syscall argument here --
        // see the module docs. Written synchronously, before this syscall
        // returns to the parent, matching real Linux's own ordering
        // guarantee.
        unsafe {
            (parent_tidptr as *mut u32).write(child_id as u32);
        }
    }

    child_id as i64
}

/// Real Linux `clone3(2)` -- the modern, `struct`-argument clone syscall a
/// current glibc's NPTL `pthread_create` actually prefers over legacy
/// `clone(2)`, found the exact ground-truth way every other gap in this
/// module was: booting a real, unmodified `java` binary far enough for
/// HotSpot to attempt its own first internal thread and watching it call
/// syscall 435 (see README item 35). Takes one pointer to a real
/// `struct clone_args` (Linux's own `include/uapi/linux/sched.h`) plus
/// that struct's size, instead of five separate register arguments:
///
/// ```text
/// offset  field
///      0  flags        (u64) -- same CLONE_* bits sys_clone already reads, still not exit_signal-packed the way clone(2)'s flags register argument is
///      8  pidfd         (u64) -- unused here, same as sys_clone never asked for one
///     16  child_tid     (u64) -- clone3's name for sys_clone's child_tidptr
///     24  parent_tid    (u64) -- clone3's name for sys_clone's parent_tidptr
///     32  exit_signal   (u64) -- unused; sys_clone never consulted this either
///     40  stack         (u64) -- the LOW address of the child's stack allocation
///     48  stack_size    (u64) -- its size in bytes
///     56  tls           (u64) -- clone3's name for sys_clone's tls register argument
/// ```
/// (`set_tid`/`set_tid_size`/`cgroup` follow at 64/72/80 in a real kernel's
/// idea of the full, current struct -- unread here, since nothing this
/// kernel does needs them; `size` only has to cover through `tls`, offset
/// 64, for every field this function actually touches).
///
/// The one genuinely easy-to-get-wrong detail, real enough to be worth its
/// own explanation rather than a one-line comment: real `clone3`'s `stack`
/// is the **bottom** (lowest address) of the child's stack allocation, the
/// *opposite* convention from legacy `clone(2)`'s `child_stack` argument,
/// which the caller already computes as the top (real x86_64 stacks grow
/// down, so "top" is the high address a real `push` would first land
/// below) -- real glibc's own `clone3` wrapper never adds `stack_size`
/// for you; the kernel is expected to. Getting this backwards would start
/// the child executing with its stack pointer at the *bottom* of its
/// allocation, one real push away from running off the end of its own
/// stack into whatever memory happens to sit right before it -- so
/// `child_stack_top` here is computed explicitly, not assumed equal to
/// the raw field.
fn sys_clone3(cl_args_ptr: u64, size: u64) -> i64 {
    // Real Linux itself enforces a minimum size (CLONE_ARGS_SIZE_VER0),
    // rejecting anything that couldn't possibly carry every field a given
    // kernel version understands; the same defensive spirit here, sized to
    // exactly what this function actually reads (through `tls`, offset
    // 64) rather than the full modern struct, so a caller built against an
    // older, smaller `clone_args` (which never had `set_tid`/
    // `set_tid_size`/`cgroup` at all) still works.
    const CLONE_ARGS_MIN_SIZE: u64 = 64;
    if size < CLONE_ARGS_MIN_SIZE {
        return EINVAL;
    }
    // Trusted pointer, same limited way as every other syscall argument
    // this module reads directly out of ring-3 memory -- see the module
    // docs.
    let field = |offset: u64| unsafe { ((cl_args_ptr + offset) as *const u64).read() };
    let flags = field(0);
    let child_tid = field(16);
    let parent_tid = field(24);
    let stack = field(40);
    let stack_size = field(48);
    let tls = field(56);

    if stack == 0 || stack_size == 0 {
        // Real Linux allows CLONE_VM-less clone3 with no stack (a
        // fork()-like call); every real caller here is thread creation,
        // which always provides one -- same honest refusal sys_clone
        // already gives a NULL child_stack.
        return EINVAL;
    }
    let child_stack_top = stack.wrapping_add(stack_size);
    do_clone(flags, child_stack_top, parent_tid, child_tid, tls)
}

/// Convert the x86_64 signed timespec to the PIT clock shared by the currently
/// supported clock IDs. User access must occur without scheduler/file locks.
fn timespec_deadline(ptr: u64, relative: bool) -> Result<u64, i64> {
    if ptr == 0 || ptr.checked_add(16).is_none() { return Err(-14); } // EFAULT
    // Existing trusted mapped-user-pointer contract; accept unaligned input.
    let (sec, nsec) = unsafe {
        ((ptr as *const i64).read_unaligned(), ((ptr + 8) as *const i64).read_unaligned())
    };
    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 { return Err(EINVAL); }
    let hz = timer::HZ as u64;
    let ticks = (sec as u64).saturating_mul(hz).saturating_add(
        ((nsec as u64) * hz + 999_999_999) / 1_000_000_000);
    Ok(if relative {
        // Current tick may already be almost over. A margin prevents positive
        // relative waits expiring early; distant deadlines must not wrap.
        timer::ticks().saturating_add(ticks).saturating_add(
            if sec != 0 || nsec != 0 { 1 } else { 0 })
    } else { ticks })
}

/// No asynchronous signal interruption yet, so successful sleeps leave rem
/// untouched. All supported clocks use clock_gettime's boot-relative PIT time.
fn sys_clock_nanosleep(clock: u64, flags: u64, request: u64) -> i64 {
    if !matches!(clock, CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME) || flags & !1 != 0 {
        return EINVAL;
    }
    let deadline = match timespec_deadline(request, flags & 1 == 0) {
        Ok(end) => end,
        Err(error) => return error,
    };
    task::sleep_until(deadline);
    0
}

/// WAIT/WAKE and WAIT_BITSET with MATCH_ANY, including deadlines. WAIT uses
/// a relative interval; WAIT_BITSET uses an absolute clock value. Realtime
/// and monotonic currently share clock_gettime's boot-relative PIT clock.
/// Selective masks remain unsupported.
///
/// On this single-core kernel syscall entry disables interrupts. The value
/// check and transition to Blocked therefore cannot race a waker. The task
/// helper releases TASKS before scheduling; do not hold it across the wait.
fn sys_futex(uaddr: u64, op: u64, val: u64, timeout: u64) -> i64 {
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    if op & FUTEX_CLOCK_REALTIME != 0 && cmd != FUTEX_WAIT_BITSET {
        return ENOSYS;
    }
    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            if uaddr & 3 != 0 {
                return EINVAL;
            }
            if cmd == FUTEX_WAIT_BITSET {
                // The sixth Linux argument is r9, saved at slot 5 of the
                // 128-byte syscall frame, exactly as in sys_mmap. Entry
                // owns this kernel stack and interrupts are still disabled.
                let mask = unsafe {
                    ((gdt::SYSCALL_KERNEL_RSP - 128 + 5 * 8) as *const u64).read()
                } as u32;
                if mask == 0 {
                    return EINVAL;
                }
                if mask != FUTEX_BITSET_MATCH_ANY {
                    return ENOSYS;
                }
            }
            let deadline = if timeout == 0 { None } else {
                match timespec_deadline(timeout, cmd == FUTEX_WAIT) {
                    Ok(end) => Some(end),
                    Err(error) => return error,
                }
            };
            // Existing syscall ABI trusts mapped user pointers. Alignment is
            // checked above; general user-memory validation remains a separate
            // kernel limitation. IF=0 keeps the mapping/value stable here.
            let current = unsafe { (uaddr as *const u32).read() };
            if current != val as u32 {
                return EAGAIN;
            }
            if task::futex_wait(uaddr, deadline) { ETIMEDOUT } else { 0 }
        }
        FUTEX_WAKE => task::futex_wake(uaddr, val as u32) as i64,
        _ => {
            println!("linux_syscall: unimplemented futex op {op:#x}");
            ENOSYS
        }
    }
}

/// Real Linux `rt_sigaction(2)`. What actually fires a handler is
/// `paging.rs`'s `try_deliver_sigsegv`, not this function -- this just
/// records/reports `(handler, flags, restorer)`, the same "install now,
/// consult later" split every other piece of per-task state here
/// (`open_files`, ...) already uses. `act`/`oldact` point at a
/// real kernel-ABI `struct k_sigaction` -- **not** the `struct sigaction`
/// shape a C program's own source sees (glibc/musl reorder the fields
/// before making the syscall); ground-truthed by hand-crafting one of
/// these directly (bypassing libc's wrapper entirely) and confirming a
/// real musl-linked binary's own handler still fired correctly against
/// it: `{ handler: u64, flags: u64, restorer: u64, mask: u64 }`, 32 bytes,
/// in that exact order. `sigsetsize` (`a2`, always 8 from every real
/// caller seen so far) is read but not consulted -- signal *masks*
/// themselves aren't tracked yet (see `sys_rt_sigprocmask`'s call site).
fn sys_rt_sigaction(sig: u64, act_ptr: u64, oldact_ptr: u64) -> i64 {
    if sig == 0 || sig >= 32 {
        return EINVAL;
    }
    if oldact_ptr != 0 {
        let (handler, flags, restorer) = task::sigaction(sig);
        unsafe {
            (oldact_ptr as *mut u64).write(handler);
            ((oldact_ptr + 8) as *mut u64).write(flags);
            ((oldact_ptr + 16) as *mut u64).write(restorer);
            ((oldact_ptr + 24) as *mut u64).write(0); // mask -- not tracked, see this function's doc comment.
        }
    }
    if act_ptr != 0 {
        let handler = unsafe { (act_ptr as *const u64).read() };
        let flags = unsafe { ((act_ptr + 8) as *const u64).read() };
        let restorer = unsafe { ((act_ptr + 16) as *const u64).read() };
        task::set_sigaction(sig, handler, flags, restorer);
    }
    0
}

/// Real Linux `rt_sigreturn(2)` -- what a delivered handler's restorer
/// trampoline (`sa_restorer`, real musl/glibc code this kernel never
/// wrote, see `paging.rs`'s `try_deliver_sigsegv`) calls the instant the
/// handler itself returns. Diverges (`-> !`) instead of returning a value
/// like every other syscall here: unlike an ordinary syscall, which
/// resumes the exact context that issued it via `sysretq` (which can't
/// restore `RCX`/`R11` -- both got clobbered by the `syscall` instruction
/// itself on the way in), this one has to resume a *completely different,
/// already-fully-known* context -- the one `paging.rs` snapshotted the
/// instant the original fault happened, `RCX`/`R11` included -- so it
/// bypasses `linux_syscall_entry`'s normal fxrstor/pop/`sysretq` epilogue
/// entirely and jumps straight into [`sigreturn_restore`]'s own `iretq`-
/// based one instead. `task::end_signal_delivery` returning `None` means
/// this task's own userspace called `rt_sigreturn` without this kernel
/// ever having delivered it a signal in the first place -- not something
/// any real program does on its own, so it's treated the same as any
/// other "the guest broke the contract" case: log it and kill the task,
/// rather than trying to invent a context to resume that was never
/// actually saved.
fn sys_rt_sigreturn() -> ! {
    unsafe extern "C" {
        fn sigreturn_restore(ctx: *const task::SavedContext) -> !;
    }
    match task::end_signal_delivery() {
        Some(ctx) => unsafe { sigreturn_restore(&ctx) },
        None => {
            println!("linux_syscall: rt_sigreturn with no signal in progress -- killing task");
            task::task_exit();
        }
    }
}

// SavedContext's fields, read by sigreturn_restore's hand-written asm
// below at fixed byte offsets (`[rdi + N]`) rather than through any
// Rust-visible accessor -- these tripwires make sure that if the struct's
// field order in task.rs ever changes, this file fails to *build* instead
// of silently reading the wrong field at runtime the next time someone
// actually hits a SIGSEGV.
const _: () = assert!(core::mem::offset_of!(task::SavedContext, rip) == 0);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, cs) == 8);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, rflags) == 16);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, rsp) == 24);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, ss) == 32);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, rax) == 40);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, rbx) == 48);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, rcx) == 56);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, rdx) == 64);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, rsi) == 72);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, rdi) == 80);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, rbp) == 88);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, r8) == 96);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, r9) == 104);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, r10) == 112);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, r11) == 120);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, r12) == 128);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, r13) == 136);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, r14) == 144);
const _: () = assert!(core::mem::offset_of!(task::SavedContext, r15) == 152);

// sigreturn_restore: rebuilds a genuine `iretq` frame straight from a
// saved SavedContext (rdi = &SavedContext, the ordinary SysV first-arg
// register) and jumps back into it, restoring *every* GPR -- unlike
// linux_syscall_resume_frame's sysretq-based epilogue just below, this
// can bring back RCX/R11 too, because iretq (unlike sysretq) never needed
// either of them for anything in the first place. rax and rdi are read
// through rdi (the base pointer) last, in that order -- rax first, since
// it's needed as scratch to build the pushed iretq frame above it, then
// rdi itself dead last, since it's the one register this whole routine
// can't afford to lose track of until every other field has already been
// read out of it.
global_asm!(
    r#"
.section .text

.global sigreturn_restore
sigreturn_restore:
    mov rax, [rdi + 32]    # ss
    push rax
    mov rax, [rdi + 24]    # rsp
    push rax
    mov rax, [rdi + 16]    # rflags
    push rax
    mov rax, [rdi + 8]     # cs
    push rax
    mov rax, [rdi + 0]     # rip
    push rax

    mov rbx, [rdi + 48]
    mov rcx, [rdi + 56]
    mov rdx, [rdi + 64]
    mov rsi, [rdi + 72]
    mov rbp, [rdi + 88]
    mov r8,  [rdi + 96]
    mov r9,  [rdi + 104]
    mov r10, [rdi + 112]
    mov r11, [rdi + 120]
    mov r12, [rdi + 128]
    mov r13, [rdi + 136]
    mov r14, [rdi + 144]
    mov r15, [rdi + 152]
    mov rax, [rdi + 40]
    mov rdi, [rdi + 80]
    iretq
"#
);

// Entered directly via the `syscall` instruction from ring 3 -- see the
// module docs for why this looks nothing like syscall.rs's interrupt-gate
// stub: no automatic stack switch, RCX/R11 already repurposed by the CPU
// for the return RIP/RFLAGS before this even starts, and exit is
// `sysretq`, not `iretq`.
global_asm!(
    r#"
.section .text

.global linux_syscall_entry
linux_syscall_entry:
    # RSP is still the *user* stack pointer here -- syscall doesn't touch
    # it. Stash it in a scratch global just long enough to copy it onto a
    # real kernel stack below; safe even though nothing's disabled
    # interrupts yet, because SFMASK already cleared IF as part of the
    # `syscall` instruction itself (see linux_syscall::init), so nothing
    # can preempt this CPU between here and the point that copy lands
    # safely on the new stack.
    mov [rip + LINUX_SYSCALL_SCRATCH_RSP], rsp
    mov rsp, [rip + SYSCALL_KERNEL_RSP]

    push qword ptr [rip + LINUX_SYSCALL_SCRATCH_RSP]  # user RSP
    push r11        # user RFLAGS (CPU-saved by `syscall`)
    push rcx        # user RIP    (CPU-saved by `syscall`)
    push rax        # syscall number -- overwritten with the return value below
    push rbx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r12
    push r13
    push r14
    push r15

    mov r11, [rip + CURRENT_FXSAVE_PTR]
    fxsave [r11]

    # Real syscall convention (rax=number, rdi,rsi,rdx,r10,r8,r9=args) ->
    # SysV call convention linux_syscall_handler expects (rdi,rsi,rdx,rcx,
    # r8,r9). Same backwards-through-the-dependency-chain shuffle
    # syscall.rs's stub uses, extended by one more argument (r8 -> r9).
    mov r9, r8
    mov r8, r10
    mov rcx, rdx
    mov rdx, rsi
    mov rsi, rdi
    mov rdi, rax

    call linux_syscall_handler
    mov [rsp + 12*8], rax    # overwrite the saved-rax(number) slot with the real return value

.global linux_syscall_resume_frame
linux_syscall_resume_frame:
    # A brand new clone()'d thread's *first* run doesn't fall through from
    # the `call` above -- it arrives here directly, via switch_to's `ret`
    # landing on this exact label (see task::spawn_clone_raw's doc comment
    # and linux_syscall.rs's own module docs). At that point RSP has
    # already been placed, by spawn_clone_raw, exactly where a 16-word
    # linux_syscall_entry frame belongs -- the same shape the *real* entry
    # path above always leaves behind at this point, just hand-built
    # instead of pushed by hardware/this asm. Either way, from here on the
    # child is indistinguishable from any other task finishing an ordinary
    # syscall: same fxrstor, same 16 pops, same sysretq, landing back in
    # ring 3 at the same RIP the parent's `syscall` instruction returned
    # to, with rax=0 (patched into this slot by sys_clone before spawning)
    # instead of whatever the parent sees.
    mov r11, [rip + CURRENT_FXSAVE_PTR]
    fxrstor [r11]

    pop r15
    pop r14
    pop r13
    pop r12
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rbx
    pop rax
    pop rcx         # user RIP, for sysretq
    pop r11         # user RFLAGS, for sysretq
    pop rsp         # switch back to the user stack in one shot
    sysretq
"#
);

#[unsafe(no_mangle)]
static mut LINUX_SYSCALL_SCRATCH_RSP: u64 = 0;
