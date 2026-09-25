//! The real ELF entry point.
//!
//! `core` (as prebuilt for the ordinary x86_64-unknown-linux-gnu host
//! target -- see the toolchain note in `Cargo.toml`) assumes SSE2 is
//! available, and uses `movups`/`movaps` to shuffle plain data around (for
//! example, copying the two-word-times-two `Option<BootloaderInfo>` this
//! kernel returns from `limine::bootloader_info()`) even when no floating
//! point math is involved. That's just how the ABI it was built for treats
//! bulk register-sized copies.
//!
//! Limine does *not* guarantee SSE is enabled when it jumps to the kernel
//! (we found this out by booting in QEMU: the very first `movups` faulted
//! with #GP, CR4.OSFXSR was 0). So before touching a single line of the
//! `kstart` Rust function, this tiny assembly stub sets:
//!   * CR0.EM = 0 (don't emulate x87/SSE in software)
//!   * CR0.MP = 1 (monitor coprocessor -- required alongside clearing EM)
//!   * CR4.OSFXSR = 1 (OS supports FXSAVE/FXRSTOR -- required for SSE)
//!   * CR4.OSXMMEXCPT = 1 (OS supports unmasked SIMD FP exceptions)
//!
//! It also forces 16-byte stack alignment before calling into Rust. The
//! SysV ABI requires RSP % 16 == 0 immediately before a `call`, and the
//! compiler relies on that to use aligned SSE stores (`movaps`) for
//! spilling register-sized values to the stack; the first one of those
//! faulted with #GP until this `and rsp, -16` was added, which means
//! Limine's initial RSP was not actually 16-byte aligned (or at least
//! shouldn't be trusted to be).
//!
//! Only once SSE is enabled and the stack is aligned does this jump into
//! `kstart`, which does everything else.

use core::arch::global_asm;

global_asm!(
    r#"
.section .text._entry, "ax"
.global _entry
.type _entry, @function
_entry:
    and rsp, -16         # Force 16-byte stack alignment (see module docs).

    mov rax, cr0
    and rax, ~(1 << 2)   # CR0.EM = 0
    or  rax, (1 << 1)    # CR0.MP = 1
    mov cr0, rax

    mov rax, cr4
    or  rax, (1 << 9) | (1 << 10)  # CR4.OSFXSR = 1, CR4.OSXMMEXCPT = 1
    mov cr4, rax

    call kstart
    # kstart never returns (`-> !`), but just in case, halt rather than
    # run off into whatever comes after in memory.
    cli
1:  hlt
    jmp 1b
"#
);
