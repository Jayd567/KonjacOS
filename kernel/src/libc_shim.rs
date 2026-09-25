//! A minimal libc shim: just enough of libc's memory-allocation API
//! (`malloc`/`free`/`calloc`/`realloc`) for freestanding C code compiled
//! into the kernel (see `build.rs`/`csrc/`) to actually run, routed
//! straight onto `heap.rs`'s existing `#[global_allocator]` rather than a
//! separate allocator living alongside it.
//!
//! This is deliberately narrow. There's no `printf`/`sprintf` (needs
//! variadic-argument handling plus a real format-string parser), no
//! `fopen`/`fread`/file I/O (would route onto `fat16.rs`, but nothing
//! needs it yet), no `string.h` beyond what `intrinsics.rs` already
//! provides for the Rust compiler's own sake (`memcpy`/`memset`/`memmove`/
//! `memcmp` -- freestanding C code calling those resolves against those
//! same symbols, no extra work needed), and no math library. Real
//! doomgeneric integration will need a good chunk of that -- this is
//! explicitly just enough to prove the C-toolchain pipeline (`build.rs`
//! compiling `csrc/cdemo.c`, linking it in, this shim backing its libc
//! calls) works at all before investing in the rest.

extern crate alloc;

use core::alloc::Layout;
use core::ptr;

use crate::heap::MIN_ALIGN;

/// Every `malloc`ed block is actually `HEADER_SIZE` bytes larger than the
/// caller asked for: a hidden `usize` right before the pointer handed
/// back, recording the *original* requested size. `free`/`realloc` read it
/// back out to reconstruct the exact `Layout` `dealloc` needs -- Rust's
/// allocator API requires the freed size to match the allocated size
/// precisely, unlike libc's `free`, which takes no size at all. Rounded up
/// to `MIN_ALIGN` so the usable region after the header stays aligned the
/// same way every other allocation from this heap is.
const HEADER_SIZE: usize = MIN_ALIGN;

const _: () = assert!(HEADER_SIZE >= core::mem::size_of::<usize>(), "MIN_ALIGN too small to hold malloc's size header");

/// Builds the `Layout` for the *underlying* allocation (header + usable
/// bytes) backing a `malloc(size)` call. Returns `None` on overflow (an
/// absurdly large `size` that would wrap the address space doing the
/// math) rather than ever handing out a too-small buffer.
fn block_layout(size: usize) -> Option<Layout> {
    let total = size.checked_add(HEADER_SIZE)?;
    Layout::from_size_align(total, MIN_ALIGN).ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn malloc(size: usize) -> *mut u8 {
    if size == 0 {
        return ptr::null_mut();
    }
    let Some(layout) = block_layout(size) else {
        return ptr::null_mut();
    };

    // Safety: `layout` has nonzero size (HEADER_SIZE alone guarantees
    // that even if `size` somehow didn't), matching what `GlobalAlloc`
    // requires.
    let raw = unsafe { alloc::alloc::alloc(layout) };
    if raw.is_null() {
        return ptr::null_mut();
    }

    // Safety: `raw` is a fresh, `MIN_ALIGN`-aligned allocation at least
    // `HEADER_SIZE` bytes long -- writing one `usize` at its start is in
    // bounds and correctly aligned.
    unsafe {
        (raw as *mut usize).write(size);
    }
    // Safety: `raw` has at least `HEADER_SIZE` bytes reserved before the
    // pointer returned to the caller, per `block_layout`.
    unsafe { raw.add(HEADER_SIZE) }
}

/// # Safety
/// `ptr` must be null, or a pointer previously returned by `malloc`/
/// `calloc`/`realloc` from this same shim that hasn't already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free(ptr_in: *mut u8) {
    if ptr_in.is_null() {
        return;
    }
    unsafe {
        free_block(ptr_in);
    }
}

/// Shared by `free`/`realloc`: recovers the original allocation's `Layout`
/// from its hidden size header and hands it back to the allocator.
///
/// # Safety
/// Same requirement as `free`: `user_ptr` must be a still-live pointer
/// this shim itself handed out.
unsafe fn free_block(user_ptr: *mut u8) {
    unsafe {
        let raw = user_ptr.sub(HEADER_SIZE);
        let size = (raw as *const usize).read();
        // `block_layout` succeeded when this block was allocated (or it
        // wouldn't exist to free), so it succeeds identically here --
        // same inputs, same computation.
        let layout = block_layout(size).expect("free: corrupted malloc header (size overflowed Layout)");
        alloc::alloc::dealloc(raw, layout);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn calloc(nmemb: usize, size: usize) -> *mut u8 {
    let Some(total) = nmemb.checked_mul(size) else {
        return ptr::null_mut();
    };
    let out = malloc(total);
    if !out.is_null() {
        // Safety: `malloc` just returned exactly `total` usable bytes at
        // `out` (or null, handled above).
        unsafe {
            ptr::write_bytes(out, 0, total);
        }
    }
    out
}

/// # Safety
/// `ptr_in` must be null, or a pointer previously returned by `malloc`/
/// `calloc`/`realloc` from this same shim that hasn't already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn realloc(ptr_in: *mut u8, new_size: usize) -> *mut u8 {
    if ptr_in.is_null() {
        return malloc(new_size);
    }
    if new_size == 0 {
        unsafe {
            free_block(ptr_in);
        }
        return ptr::null_mut();
    }

    // Safety: `ptr_in` is a live malloc'd pointer (caller's obligation),
    // so its header is exactly where every other malloc'd block's is.
    let old_size = unsafe { (ptr_in.sub(HEADER_SIZE) as *const usize).read() };

    let new_ptr = malloc(new_size);
    if new_ptr.is_null() {
        // Real `realloc` leaves the original block intact on failure.
        return ptr::null_mut();
    }

    let copy_len = old_size.min(new_size);
    // Safety: `ptr_in` has `old_size` valid bytes, `new_ptr` has `new_size`
    // (>= copy_len) freshly allocated bytes, and the two can't overlap --
    // they're separate allocations.
    unsafe {
        ptr::copy_nonoverlapping(ptr_in, new_ptr, copy_len);
        free_block(ptr_in);
    }
    new_ptr
}

// --- ctype.h ------------------------------------------------------------
//
// Plain ASCII classification, exactly what any libc's "C" locale gives
// you. Nothing kernel-specific here.

#[unsafe(no_mangle)]
pub extern "C" fn isalpha(c: i32) -> i32 {
    (c as u8 as char).is_ascii_alphabetic() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn isdigit(c: i32) -> i32 {
    (c >= b'0' as i32 && c <= b'9' as i32) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn isalnum(c: i32) -> i32 {
    (isalpha(c) != 0 || isdigit(c) != 0) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn isspace(c: i32) -> i32 {
    matches!(c as u8, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn isupper(c: i32) -> i32 {
    (c >= b'A' as i32 && c <= b'Z' as i32) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn islower(c: i32) -> i32 {
    (c >= b'a' as i32 && c <= b'z' as i32) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn isxdigit(c: i32) -> i32 {
    (isdigit(c) != 0 || (c >= b'a' as i32 && c <= b'f' as i32) || (c >= b'A' as i32 && c <= b'F' as i32)) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn ispunct(c: i32) -> i32 {
    ((c as u8 as char).is_ascii_punctuation()) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn iscntrl(c: i32) -> i32 {
    ((c as u8 as char).is_ascii_control()) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn isprint(c: i32) -> i32 {
    (c >= 0x20 && c < 0x7F) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn toupper(c: i32) -> i32 {
    if islower(c) != 0 { c - (b'a' as i32) + (b'A' as i32) } else { c }
}

#[unsafe(no_mangle)]
pub extern "C" fn tolower(c: i32) -> i32 {
    if isupper(c) != 0 { c - (b'A' as i32) + (b'a' as i32) } else { c }
}

// --- stdlib.h extras ------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn abs(n: i32) -> i32 {
    n.wrapping_abs()
}

#[unsafe(no_mangle)]
pub extern "C" fn labs(n: i64) -> i64 {
    n.wrapping_abs()
}

/// # Safety
/// `s` must be non-null and point at a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atoi(s: *const u8) -> i32 {
    unsafe { strtol_impl(s) as i32 }
}

/// # Safety
/// `s` must be non-null and point at a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atof(s: *const u8) -> f64 {
    unsafe { strtod_impl(s) }
}

/// # Safety
/// `s` must be non-null and point at a valid NUL-terminated C string.
/// `endptr` is written the address of the first unconsumed byte if
/// non-null. `base` other than 10 isn't needed by anything in the
/// doomgeneric core file set, so only base 10 (and the base-0 "detect
/// decimal" case) is actually implemented; anything else falls back to
/// base 10 as well rather than failing outright.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtol(s: *const u8, endptr: *mut *mut u8, _base: i32) -> i64 {
    let (value, consumed) = unsafe { strtol_impl_ex(s) };
    if !endptr.is_null() {
        unsafe {
            *endptr = s.add(consumed) as *mut u8;
        }
    }
    value
}

/// # Safety
/// `s` must be non-null and point at a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtod(s: *const u8, endptr: *mut *mut u8) -> f64 {
    let value = unsafe { strtod_impl(s) };
    if !endptr.is_null() {
        // Good enough for the (currently nonexistent) callers of this:
        // point past the whole string rather than tracking the exact
        // stopping byte, since nothing in the doomgeneric core files
        // actually calls strtod.
        unsafe {
            let mut p = s;
            while *p != 0 {
                p = p.add(1);
            }
            *endptr = p as *mut u8;
        }
    }
    value
}

unsafe fn strtol_impl(s: *const u8) -> i64 {
    unsafe { strtol_impl_ex(s).0 }
}

unsafe fn strtol_impl_ex(s: *const u8) -> (i64, usize) {
    let mut i = 0usize;
    unsafe {
        while *s.add(i) == b' ' || *s.add(i) == b'\t' {
            i += 1;
        }
        let mut neg = false;
        if *s.add(i) == b'-' {
            neg = true;
            i += 1;
        } else if *s.add(i) == b'+' {
            i += 1;
        }
        let mut value: i64 = 0;
        while (*s.add(i)).is_ascii_digit() {
            value = value * 10 + (*s.add(i) - b'0') as i64;
            i += 1;
        }
        (if neg { -value } else { value }, i)
    }
}

unsafe fn strtod_impl(s: *const u8) -> f64 {
    let mut i = 0usize;
    unsafe {
        while *s.add(i) == b' ' || *s.add(i) == b'\t' {
            i += 1;
        }
        let mut neg = false;
        if *s.add(i) == b'-' {
            neg = true;
            i += 1;
        } else if *s.add(i) == b'+' {
            i += 1;
        }
        let mut value: f64 = 0.0;
        while (*s.add(i)).is_ascii_digit() {
            value = value * 10.0 + (*s.add(i) - b'0') as f64;
            i += 1;
        }
        if *s.add(i) == b'.' {
            i += 1;
            let mut frac = 0.1;
            while (*s.add(i)).is_ascii_digit() {
                value += (*s.add(i) - b'0') as f64 * frac;
                frac *= 0.1;
                i += 1;
            }
        }
        if neg { -value } else { value }
    }
}

/// `exit`/`abort`: DOOM runs as its own kernel task (see
/// `doomgeneric_konjac.c`/the `doom` shell command), so "exiting the
/// process" can't mean halting the whole machine -- that would take the
/// shell and every other task down with it. `task::exit_current` (see
/// `task.rs`) is the real primitive this needs: it marks the task
/// terminated, switches away from it for good, and its stack/fxsave area
/// get freed and the slot reused on a later scheduling pass. Since DOOM
/// (or whatever else calls this) owns the framebuffer for as long as it's
/// drawing to it, its last rendered frame would otherwise just sit there
/// forever with nothing left running to clear it -- so this wipes the
/// console back to blank first, which is also what makes the shell visibly
/// "get control back" the moment DOOM quits, `I_Quit`'s `exit(0)` included
/// (see `d_main.c`/`i_system.c`), rather than that only being true in some
/// abstract scheduler sense.
#[unsafe(no_mangle)]
pub extern "C" fn exit(status: i32) -> ! {
    crate::sprintln!("[doom] exit({}) called -- returning control to the shell.", status);
    crate::console::CONSOLE.lock().clear();
    crate::println!("[doom exited with status {status} -- back to the shell]");
    crate::task::exit_current();
}

#[unsafe(no_mangle)]
pub extern "C" fn abort() -> ! {
    crate::sprintln!("[doom] abort() called -- returning control to the shell.");
    crate::console::CONSOLE.lock().clear();
    crate::println!("[doom aborted -- back to the shell]");
    crate::task::exit_current();
}

/// A no-op: console writes (`cfile.rs`'s `fwrite`/`fputs` on `stdout`/
/// `stderr`) already go straight to `print!` with nothing buffered, and
/// real file writes are only ever flushed to disk as a whole at `fclose`
/// (see `cfile.rs`'s doc comment -- `fat16.rs` has no partial-write
/// primitive to flush against mid-stream anyway), so there's never
/// anything for this to actually do.
#[unsafe(no_mangle)]
pub extern "C" fn fflush(_stream: *mut u8) -> i32 {
    0
}

/// No environment variables exist in KonjacOS; every lookup simply
/// misses, matching how a real libc's `getenv` behaves for an unset
/// variable. Everything in the doomgeneric core files that calls this
/// (`DOOMWADPATH`/`DOOMWADDIR`/`TEMP`) already has a fallback path for
/// exactly this case.
#[unsafe(no_mangle)]
pub extern "C" fn getenv(_name: *const u8) -> *const u8 {
    ptr::null()
}

/// No shell/process-spawning facility exists in KonjacOS. The only real
/// caller of this (`i_system.c`'s zenity error-dialog helper) treats any
/// nonzero return as "not available" and falls back to a plain
/// message-only error path, so failing here is completely safe.
#[unsafe(no_mangle)]
pub extern "C" fn system(_command: *const u8) -> i32 {
    -1
}

/// `M_MakeDirectory`'s only caller creates a save-game directory; there's
/// no save-game support yet (`remove`/`rename` are stubbed the same way
/// below), so this just needs to not crash. Reporting success keeps
/// callers on their normal path instead of an error branch that was never
/// exercised against a real filesystem-backed implementation.
#[unsafe(no_mangle)]
pub extern "C" fn mkdir(_path: *const u8, _mode: u32) -> i32 {
    0
}

/// Save-game support (`G_DoSaveGame`'s temp-file-then-rename dance) isn't
/// implemented yet -- these are only reached from the save/load menu, not
/// from anything on the startup path, so failing them is safe: DOOM's own
/// code already checks the return value and reports "couldn't save" back
/// to the player rather than crashing.
#[unsafe(no_mangle)]
pub extern "C" fn remove(_path: *const u8) -> i32 {
    -1
}

#[unsafe(no_mangle)]
pub extern "C" fn rename(_oldpath: *const u8, _newpath: *const u8) -> i32 {
    -1
}

/// A single global `errno`, good enough for this kernel's current
/// single-threaded-per-call-site C usage (see errno.h's shim comment).
#[unsafe(no_mangle)]
pub static mut errno: i32 = 0;

/// qsort isn't called anywhere in the doomgeneric core file set (grepped
/// and confirmed), so this is a correct-but-unoptimized insertion sort
/// rather than a real quicksort -- there's no reason to invest more until
/// something actually exercises it.
///
/// # Safety
/// `base` must point at `nmemb * size` valid, contiguous bytes; `compar`
/// must be a valid comparison function pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qsort(
    base: *mut u8,
    nmemb: usize,
    size: usize,
    compar: extern "C" fn(*const u8, *const u8) -> i32,
) {
    if nmemb < 2 || size == 0 {
        return;
    }
    unsafe {
        for i in 1..nmemb {
            let mut j = i;
            while j > 0 {
                let a = base.add(j * size);
                let b = base.add((j - 1) * size);
                if compar(a, b) < 0 {
                    for k in 0..size {
                        ptr::swap(a.add(k), b.add(k));
                    }
                    j -= 1;
                } else {
                    break;
                }
            }
        }
    }
}

/// No entropy source is wired up yet; a fixed linear congruential
/// generator is good enough for anything in the doomgeneric core files
/// that might call this (nothing currently does -- grepped and
/// confirmed), and is deterministic, which is actually convenient for
/// testing.
static mut RAND_STATE: u32 = 12345;

#[unsafe(no_mangle)]
pub extern "C" fn rand() -> i32 {
    unsafe {
        RAND_STATE = RAND_STATE.wrapping_mul(1103515245).wrapping_add(12345);
        ((RAND_STATE >> 16) & 0x7fff) as i32
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn srand(seed: u32) {
    unsafe {
        RAND_STATE = seed;
    }
}
