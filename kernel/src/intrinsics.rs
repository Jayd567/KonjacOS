//! Freestanding replacements for the handful of C-ABI symbols that the Rust
//! compiler normally expects a libc to provide (`memcpy`, `memset`, ...) and
//! for the unwinder personality routine.
//!
//! Because we compile against the `x86_64-unknown-linux-gnu` target (see
//! `.cargo/config.toml`), `compiler_builtins` assumes libc will supply
//! `mem*`, since on a normal Linux binary it always would. We have no libc,
//! so we define them ourselves. `memcpy` (and `memmove`'s forward case)
//! copies 64 bytes at a time through SSE2 registers, and `memset` fills 8
//! bytes at a time with `rep stosq`; the last few bytes go one at a time.
//! (They used `rep movsb`/`rep stosb` at first, which is fast on hardware
//! with "enhanced rep movsb" but crawls under QEMU without acceleration,
//! which emulates it byte by byte: 36 MB/s for `memcpy` against 1.1 GB/s
//! for the SSE2 loop. Every file read and every frame the desktop draws
//! goes through these.) Everything else is a plain byte loop.
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
    // Forwards only, 64 bytes per round (unaligned loads and stores are
    // fine), which also makes it a correct `memmove` whenever `dest < src`.
    // The ABI guarantees the direction flag is clear for the `rep movsb`
    // tail.
    let bulk = n & !63;
    unsafe {
        let (mut d, mut s) = (dest, src);
        if bulk != 0 {
            core::arch::asm!(
                "2:",
                "movdqu xmm0, [rsi]",
                "movdqu xmm1, [rsi + 16]",
                "movdqu xmm2, [rsi + 32]",
                "movdqu xmm3, [rsi + 48]",
                "movdqu [rdi], xmm0",
                "movdqu [rdi + 16], xmm1",
                "movdqu [rdi + 32], xmm2",
                "movdqu [rdi + 48], xmm3",
                "add rsi, 64",
                "add rdi, 64",
                "sub rcx, 64",
                "jnz 2b",
                inout("rcx") bulk => _,
                inout("rdi") d,
                inout("rsi") s,
                out("xmm0") _,
                out("xmm1") _,
                out("xmm2") _,
                out("xmm3") _,
                options(nostack)
            );
        }
        core::arch::asm!(
            "rep movsb",
            inout("rcx") n - bulk => _,
            inout("rdi") d => _,
            inout("rsi") s => _,
            options(nostack, preserves_flags)
        );
    }
    dest
}

/// # Safety
/// `dest` must be valid for `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut u8, c: i32, n: usize) -> *mut u8 {
    // Eight bytes at a time, then the rest one at a time.
    let pattern = (c as u8 as u64) * 0x0101_0101_0101_0101;
    unsafe {
        core::arch::asm!(
            "rep stosq",
            "mov rcx, {tail}",
            "rep stosb",
            tail = in(reg) n & 7,
            inout("rcx") n / 8 => _,
            inout("rdi") dest => _,
            in("rax") pattern,
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

/// Unwinding never happens in this kernel (`panic = "abort"`), but the
/// precompiled `alloc` that stable Rust ships was built for unwinding, so
/// some of its functions (`format!`, `String::from_utf8_lossy`, ...) carry
/// cleanup paths that call this. Those paths only run while unwinding a
/// panic, which an abort never does, so this is never called; it exists so
/// the kernel links. (Before it existed, those functions had to be
/// avoided; see the README's toolchain notes.)
#[unsafe(no_mangle)]
pub extern "C" fn _Unwind_Resume() -> ! {
    panic!("_Unwind_Resume called: something tried to unwind");
}
