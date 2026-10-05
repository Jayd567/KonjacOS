//! Freestanding replacements for the handful of C-ABI symbols that the Rust
//! compiler normally expects a libc to provide (`memcpy`, `memset`, ...) and
//! for the unwinder personality routine.
//!
//! Because we compile against the `x86_64-unknown-linux-gnu` target (see
//! `.cargo/config.toml`), `compiler_builtins` assumes libc will supply
//! `mem*`, since on a normal Linux binary it always would. We have no libc,
//! so we define them ourselves. `memcpy`/`memset` (and `memmove`'s forward
//! case) use the CPU's own `rep movsb`/`rep stosb` string instructions,
//! which are both simple and fast -- the desktop compositor moves whole
//! frames through them. Everything else is a plain byte loop.
//!
//! `strlen` joined this list once `cfile.rs` started using
//! `core::ffi::CStr::from_ptr` (part of the DOOM-porting groundwork's libc
//! shim, for reading C-string arguments like `fopen`'s `path`): its stdlib
//! implementation calls out to an actual `strlen` symbol rather than
//! scanning the bytes itself, same underlying reason as every other symbol
//! here -- `core`/`alloc` were built assuming a real libc supplies it.

#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}

/// # Safety
/// `dest` and `src` must each be valid for `n` bytes and must not overlap
/// (use `memmove` if they might).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    // The ABI guarantees the direction flag is clear on entry, so this
    // copies forwards -- which also makes it a correct `memmove` whenever
    // `dest < src`.
    unsafe {
        core::arch::asm!(
            "rep movsb",
            inout("rcx") n => _,
            inout("rdi") dest => _,
            inout("rsi") src => _,
            options(nostack, preserves_flags)
        );
    }
    dest
}

/// # Safety
/// `dest` must be valid for `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut u8, c: i32, n: usize) -> *mut u8 {
    unsafe {
        core::arch::asm!(
            "rep stosb",
            inout("rcx") n => _,
            inout("rdi") dest => _,
            in("al") c as u8,
            options(nostack, preserves_flags)
        );
    }
    dest
}

/// # Safety
/// `dest` and `src` must each be valid for `n` bytes. Unlike `memcpy`, they
/// are allowed to overlap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dest as usize) < (src as usize) {
        unsafe { memcpy(dest, src, n) }
    } else {
        // Overlapping with `dest` above `src`: copy backwards. Done as a
        // byte loop rather than `std; rep movsb` -- an interrupt landing
        // mid-copy would otherwise run its handler with the direction flag
        // set, which every other `rep` in the kernel assumes is clear.
        let mut i = n;
        while i != 0 {
            i -= 1;
            unsafe {
                *dest.add(i) = *src.add(i);
            }
        }
        dest
    }
}

/// # Safety
/// `a` and `b` must each be valid for `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let (x, y) = unsafe { (*a.add(i), *b.add(i)) };
        if x != y {
            return i32::from(x) - i32::from(y);
        }
        i += 1;
    }
    0
}

/// # Safety
/// `s` must be valid for at least `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    unsafe { memcmp(a, b, n) }
}

/// # Safety
/// `s` must point at a valid, NUL-terminated byte string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strlen(s: *const u8) -> usize {
    let mut n = 0;
    unsafe {
        while *s.add(n) != 0 {
            n += 1;
        }
    }
    n
}
