# Toolchain notes


This kernel is built with **stable Rust**, no `rustup target add`, no
nightly, no `-Z build-std`. That's unusual for OS dev in Rust -- most
tutorials (including the well-known "Writing an OS in Rust" blog) have you
install nightly and a custom `x86_64-unknown-none` target. That path needs
downloading a new target's `core`/`alloc` build from `static.rust-lang.org`,
which isn't always available (it wasn't in the sandbox this was built in).

The trick used instead: compile for the *host* target
(`x86_64-unknown-linux-gnu`, which `rustup` already has `core`/`alloc` for)
as `#![no_std]`, and hand the linker a completely different, freestanding
memory layout via `linker.ld`. The generated machine code is identical
either way -- only the Rust-level `target_os` cfg differs, and this kernel
never reads it. This is *not* "running on Linux" in any sense -- there's no
libc, no syscalls, no Linux code anywhere in the binary; it's a Limine-only
naming coincidence. See the comments in `kernel/Cargo.toml` and
`kernel/.cargo/config.toml` for the full flag-by-flag rationale.

Consequences of this choice, and where they show up:

* **No libc, so no `memcpy`/`memset`/`memmove`/`memcmp`.** The
  `compiler_builtins` crate assumes libc provides these on a "linux-gnu"
  target. `intrinsics.rs` provides simple byte-at-a-time versions instead.
* **`x86_64-unknown-linux-gnu`'s prebuilt `core` assumes SSE2 is available**
  (that's the default baseline for the target) and uses it for plain bulk
  data copies, not just float math. Limine does *not* guarantee SSE is
  enabled on kernel entry -- this was caught by actually booting in QEMU
  (an early `movups` faulted with #GP because `CR4.OSFXSR` was 0).
  `boot.rs`'s `_entry` stub enables it before anything else runs.
* Relatedly, **the stack must be 16-byte aligned before calling into Rust**
  (the SysV ABI requires it, and the compiler relies on it for aligned SSE
  stores). Limine's initial stack alignment shouldn't be trusted, so
  `_entry` does `and rsp, -16` defensively.
* **No `limine` crate dependency.** The current version on crates.io needs
  a nightly-only feature (`ptr_metadata`). `limine.rs` hand-translates the
  handful of protocol structs this kernel needs, straight from
  `limine/limine.h`. This also means: if you add a new Limine feature
  request later (modules, SMP, RSDP, ...), you'll want to translate its
  struct from `limine.h` the same way, or switch to the crate once you're
  on nightly.
* **The red zone used to not be disabled** (unlike a real bare-metal
  target, which sets `disable-redzone=true` by default) -- with interrupts
  firing continuously (the 100Hz timer), a leaf function using it could in
  principle have it clobbered by an interrupt arriving mid-leaf. Closed as
  part of the DOOM-porting groundwork (see the roadmap below): `.cargo/
  config.toml` now passes `-C no-redzone` explicitly, and every C
  compilation (`build.rs`) passes the matching `-mno-red-zone`.
* **Avoid `alloc::format!`.** It links fine syntactically but fails at link
  time with `undefined symbol: _Unwind_Resume` -- the prebuilt `alloc.rlib`
  in the stable sysroot was itself compiled assuming `panic = "unwind"`,
  and `format!`'s internals pull in an unwinding path from it that has
  nothing to resolve against under this kernel's `panic = "abort"`. Plain
  `write!`/`print!`/`println!` (going through `core::fmt::Write`, as
  `console.rs`'s macros already do) don't hit this, so they're the way to
  build formatted output here; found while wiring up `apex.rs`'s prompts.
  **`alloc::string::String::from_utf8_lossy` hits the exact same
  `_Unwind_Resume` wall** -- found again while wiring up `cfile.rs`'s C
  string handling. `cfile.rs` has its own small hand-rolled, ASCII-only
  lossy-bytes-to-`String` helper instead; good enough for C strings that
  are ASCII in every case that actually reaches this kernel.
* **The heap's minimum alignment is 16, not 8**, specifically because
  `task.rs` allocates each kernel thread's `fxsave`/`fxrstor` scratch area
  on the heap, and both instructions fault on anything less than 16-byte
  aligned. `heap.rs`'s allocator rejects any request for *more* alignment
  than its `MIN_ALIGN` constant, so this had to be raised rather than
  worked around per-allocation -- worth remembering if a future allocation
  ever needs stricter alignment still (SIMD types wider than SSE, say).
* **`mov reg, symbol` is ambiguous in GNU as's Intel-syntax mode** when
  `symbol` is a `.set`/`=` absolute constant (as opposed to a real label):
  it assembles as a *memory load from that address*, not "load this
  constant value" -- caught by `usermode.rs`'s demo program page-faulting
  on boot at the exact address its own message length happened to equal.
  Fixed by loading from an explicit `[rip + label]` quadword instead of a
  bare symbol; worth remembering for any future hand-written `global_asm!`
  that wants to load a compile-time constant rather than dereference one.

If you later install a nightly toolchain plus the `rust-src` component and
want the more common `x86_64-unknown-none` + `build-std` setup instead, the
change is small: point `.cargo/config.toml`'s `target` at
`x86_64-unknown-none`, add a `[unstable] build-std = ["core", "alloc"]`
section, and you can likely delete `intrinsics.rs` and switch to the real
`limine` crate (this also fixes the red-zone gap above for free, since that
target disables it by default). Everything else (the linker script,
`boot.rs`'s SSE-enable dance, the protocol bindings) still applies -- those
aren't consequences of the stable-toolchain trick, they're just true of
Limine kernels in general.

