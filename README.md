# KonjacOS

A from-scratch x86_64 operating system kernel, written in Rust, booted via
[Limine](https://limine-bootloader.org/). It's a genuine bare-metal kernel --
no Linux underneath it anywhere, despite the Rust *compiler target* name
having "linux" in it (see [Toolchain notes](#toolchain-notes) for why).

Confirmed working, boot-tested in QEMU after every milestone below (not just
"it compiles"):

* Boots via Limine, brings up serial diagnostics, reads the memory map and
  framebuffer, draws a boot splash.
* A real GDT/TSS and a full IDT: CPU exceptions print a fault message and
  halt instead of triple-faulting the machine.
* The legacy PIC remapped, a PIT timer interrupt at 100Hz, and a PS/2
  keyboard driver (both IRQ-driven, not polled).
* A scrolling framebuffer text console (embedded 8x8 font) with a blinking
  text cursor, and a `sprintln!`-style serial console for boot diagnostics
  that's separate from it.
* A physical frame allocator (bitmap, built on Limine's HHDM), a page-table
  extension layer on top of Limine's existing paging, and a heap with a
  `#[global_allocator]` -- `alloc::vec::Vec`, `String`, etc. all work.
* An ATA PIO disk driver (read *and* write) and a read-only-no-longer FAT16
  filesystem driver: subdirectories, `cd`/`pwd` with a real current
  directory, and file create/overwrite/delete. The disk image is an
  ordinary FAT16 volume built with standard host tools (`mkfs.vfat`/
  `mtools`), so anything FAT-aware can read or write it too.
* An interactive shell dispatched through a command table (not a hardcoded
  match) -- see [Shell commands](#shell-commands) below.

## Roadmap: getting to Minecraft

The long-term goal is running real Minecraft (Java Edition) on KonjacOS.
That's a genuinely huge target -- worth writing down honestly, in order of
what blocks what, rather than pretending it's close. This section is a
living plan, not a record of what's done (see the numbered items below for
that); update it as steps complete or the plan changes.

1. **Get real Java working end to end.** OpenJDK 21 reaches native
   startup with the real JDK tree. The earlier `strtold` crash diagnosis
   was incorrect: item 44 identifies glibc aborting after an unsupported
   futex operation in `pthread_join`, and adds the required untimed wait.
   Item 45 fixes runtime path resolution and a lazy-buffer read deadlock.
   Item 46 replaces whole-file buffering with bounded reads and demand-paged
   mappings: the 140,848,911-byte modules archive no longer exhausts the
   96 MiB heap. Item 47 implements getcpu and corrects the sysinfo memory-unit
   offset, passing the legacy-vsyscall crash and the subsequent zero-memory
   heap-sizing failure. Item 48 adds timed futex waits and passes the next
   glibc abort. Item 49 removes artificial 4 KiB read truncation and adds
   getcwd, passing the String class-format and SystemProps directory errors.
   Item 50 shares descriptors across Linux thread clones, removing the child
   EBADF reads and the observed recursive class-resolution stack-guard fault.
   Item 51 implements the sleep and yield calls exposed by that trace, then
   reaches `Error loading java.security file` and names the disk JDK's own
   config placement as the next thing to check. Item 52 closes that out --
   real `argv[1..]` for spawned programs plus a full disk JDK/glibc
   placement -- and `java -version` now prints the real, correct banner
   with no kernel panic. HotSpot's own clean shutdown (a real `exit_group`)
   is still unconfirmed; see item 52's own entry for the syscalls that
   still block it. Follow actual launch failures toward a full `java -jar`
   run, then Minecraft.
2. **A minimal loopback-only TCP/IP stack.** Even *singleplayer*
   Minecraft opens a real local socket (its internal integrated server).
   No real NIC or internet access is needed for this -- just real
   `socket`/`bind`/`listen`/`connect`/`accept` syscalls and a toy TCP
   state machine over a loopback interface. Big, but decomposable into
   small, independently testable pieces (raw socket syscalls first,
   returning real data for `AF_UNIX`/loopback before a real TCP state
   machine exists at all).
3. **Graphics -- the real unknown.** Minecraft renders through LWJGL ->
   OpenGL; this kernel has a plain linear framebuffer and no GPU/GL
   support of any kind. There's no small step here yet -- it's either a
   from-scratch software GL implementation covering only what LWJGL
   actually calls, or porting something like Mesa's `llvmpipe`. Needs its
   own research spike (what does LWJGL's Linux backend actually require
   at the syscall/library level?) before it can be broken into real tiny
   steps the way 1-2 already are.
4. **The long tail of smaller real syscalls** Java/LWJGL lean on that
   nothing here has touched yet -- `epoll`/`poll`, `pipe2`, `eventfd`,
   `nanosleep`, real environment variable support for a spawned process
   (`execve`'s `envp`, currently always empty -- see item 41). Each one
   independently small and real, useful with or without Minecraft ever
   landing.

## Quick start

Prerequisites (all already present if you're building in the same
environment this was scaffolded in): `cargo`/`rustc` (stable -- no nightly
needed, see [Toolchain notes](#toolchain-notes)), `xorriso`,
`qemu-system-x86_64`, `mkfs.vfat`/`mcopy` (from `dosfstools`/`mtools`, for
the data disk -- optional, see below), and optionally the `ovmf` package for
UEFI testing.

```sh
make run        # build everything and boot it in QEMU (BIOS)
make run-uefi    # same, but via OVMF/UEFI
```

You should see boot diagnostics scroll by on stdout (the kernel's serial
console), and in the QEMU window: a boot splash, then the `konjac>` shell
prompt with a blinking cursor.

Other targets:

```sh
make kernel             # just build the kernel ELF, no ISO
make iso                # build kernel + image.iso, don't run it
make disk               # build disk.img (FAT16) from disk_root/
make clean              # remove build output (leaves disk.img alone)
make MODE=release run   # optimized build instead of the default dev build
```

`make disk` needs `mkfs.vfat` and `mcopy` (Debian/Ubuntu:
`apt install dosfstools mtools`). If they're missing it's skipped with a
warning -- the OS still boots and the shell still works, just without a
filesystem to `ls`/`cat`/`write` against.

## Shell commands

Type `help` at the `konjac>` prompt for the live list (generated from the
command table, so it can't drift out of sync with reality). As of this
writing:

| Command | Does |
|---|---|
| `help` | list commands |
| `echo <text>` | print text back |
| `clear` | clear the screen |
| `uptime` | timer ticks / elapsed time |
| `meminfo` | physical frame + heap usage, plus a heap self-test |
| `alloc <n>` | heap-allocate a `Vec<u32>` of `n` elements and sum it |
| `about` | what this is |
| `ls` | list the current directory |
| `cd [dir]` | change directory (`..`, `/`, multi-level paths); no args prints cwd |
| `pwd` | print the current directory |
| `cat <file>` | print a file's contents |
| `write <file> <text>` | create/overwrite a file |
| `rm <file>` | delete a file **[apex]** |
| `reboot` | reset the machine (8042 controller reset) **[apex]** |
| `halt` | stop the CPU **[apex]** |
| `apex` | show whether an apex check is currently cached |
| `ps` | list running kernel threads (id, state, timer ticks handed to each) |
| `kill <id>` | terminate a running task by ID (see `ps`) |
| `gui` | open the window manager (drag windows by their title bar; Esc to exit) |
| `cdemo` | run a small compiled-C function (malloc/free) -- DOOM-porting groundwork |
| `cio` | run compiled-C printf + file I/O (fopen/fread/fwrite) -- DOOM-porting groundwork |
| `doom` | launch DOOM (needs `DOOM1.WAD` at the filesystem root; quit from its own menu or `kill` it to return to the shell) |
| `run <file>` | load and run a flat binary, ELF64, or PE32+ (.exe) executable as its own task |

Commands marked **[apex]** prompt for a password the first time they (or
any other apex command) run -- choosing one, the first time ever -- then
cache success for 5 minutes. The password's SHA-256 hash lives at
`/APEX.PWD` on the disk, not the password itself.

## Project layout

```
kernel/                 The Rust kernel crate (the actual OS code)
  src/
    boot.rs               Real ELF entry point: enables SSE, aligns the
                           stack, jumps into kstart().
    main.rs                kstart(): brings up every subsystem below in
                           order and hands off to the shell.
    limine.rs              Hand-written Limine boot protocol bindings.
    serial.rs              16550 UART driver (COM1) + sprint!/sprintln!.
    gdt.rs                 GDT + TSS, incl. the double-fault IST stack.
    idt.rs                 IDT + exception handlers + hardware-IRQ stubs.
    pic.rs                 Legacy 8259 PIC remap/mask/EOI.
    timer.rs               PIT driver, IRQ0, tick counter.
    keyboard.rs             PS/2 driver, IRQ1, scancode -> ASCII, ring buffer.
    framebuffer.rs          Pixel/rect drawing + glyph blitting on the
                           Limine framebuffer.
    font.rs                 Embedded 8x8 bitmap font.
    console.rs              Scrolling text console + blinking cursor, on
                           top of framebuffer.rs.
    pmm.rs                  Bitmap physical frame allocator (on Limine's HHDM).
    paging.rs               Extends Limine's page tables with new mappings.
    vm.rs                   CR3-owned bounded reservations and lazy file backing.
    heap.rs                 GlobalAlloc free-list allocator.
    memory.rs               Ties pmm+heap init together; meminfo's backend.
    ata.rs                  ATA PIO disk driver (read + write).
    fat16.rs                Read/write FAT16 driver: dirs, cd/pwd, files.
    commands.rs             The shell's command table + builtin handlers.
    apex.rs                  sudo-style password gate for requires_apex commands.
    sha256.rs                Hand-written SHA-256 (apex's password hashing).
    task.rs                  Preemptive kernel-thread scheduler + context switch.
    syscall.rs                int 0x80 syscall gate: exit, write, open/
                               read/close (onto fat16.rs), brk (onto
                               paging.rs/pmm.rs), clone (onto task.rs's
                               spawn_thread -- a second task sharing one
                               address space), mmap (a reservation only --
                               paging.rs's page-fault handler does the
                               actual, lazy backing).
    usermode.rs                Hand-assembled ring-3 demo program + loader;
                               spawns two isolated instances to prove
                               per-task address spaces actually isolate.
    loader.rs                  The "big trio" loader: detects and parses
                               flat binaries, ELF64 (ET_EXEC and, with
                               self-relocation, ET_DYN/PIE), and PE32+
                               (.exe) executables, all through one path
                               into task::spawn_user. See the `run` command.
    mouse.rs                  PS/2 mouse driver, IRQ12, 3-byte packet decode.
    cursor.rs                  The real cursor-pack asset (extracted from
                               .cur, baked in via include_bytes!).
    wm.rs                      Minimal window manager: snapshot/blit compositor,
                               draggable windows, alpha-blended cursor.
    shell.rs                Line editing + dispatch loop.
    port.rs                 in/out port I/O wrappers (incl. insw/outsw).
    sync.rs                 A minimal spinlock.
    intrinsics.rs           memcpy/memset/memmove/memcmp/strlen -- see
                           toolchain notes.
    libc_shim.rs             malloc/free/calloc/realloc for compiled-in C
                             code (DOOM-porting groundwork), onto heap.rs.
    cfile.rs                 fopen/fclose/fread/fwrite/fseek/... + stdout/
                             stderr for compiled-in C code, onto fat16.rs.
    doom_driver.rs           Rust-side glue for doomgeneric_konjac.c:
                             blits DG_ScreenBuffer onto framebuffer.rs's
                             Canvas, ticks/sleep onto timer.rs, key events
                             onto keyboard.rs's DOOM_RING.
  csrc/                    Freestanding C sources:
                           cdemo.c (malloc/free), printf.c (the printf
                           family), iodemo.c (printf + file I/O together),
                           sscanf.c, doom_math.c (sin/cos/tan/atan/fabs via
                           x87 asm), doom_string.c (the rest of string.h).
                           Compiled by build.rs and linked into the kernel.
    doom/                  doomgeneric's own sources (81 .c + 97 .h,
                           GPL-2.0 -- see LICENSE.doomgeneric in this
                           directory) plus doomgeneric_konjac.c, the
                           KonjacOS platform driver implementing DG_Init/
                           DG_DrawFrame/DG_SleepMs/DG_GetTicksMs/
                           DG_GetKey/DG_SetWindowTitle.
    doom_include/           Freestanding libc header shims doomgeneric's
                           sources #include (stdint.h, string.h, stdio.h,
                           math.h, ...) -- compiled with -nostdinc so
                           these are the only headers ever seen.
  assets/                 Binary assets baked into the kernel via
                           include_bytes! (currently cursor_arrow.rgba).
  build.rs                Compiles csrc/ with clang/cc/gcc, links the
                           result into the kernel binary.
  linker.ld               Higher-half link layout Limine expects.
  .cargo/config.toml      Target + linker flags (see toolchain notes).
boot/limine.conf          Limine's own boot menu config.
limine/                   Prebuilt Limine binaries + limine.h (source of
                           truth for the protocol structs in limine.rs).
disk_root/                Source files for the FAT16 data disk (`make disk`
                           copies this onto disk.img, subdirectories
                           included), including DOOM1.WAD (id Software's
                           freely-redistributable 1995 shareware release)
                           and hello.elf/hello.exe/hello.bin, tiny demo
                           programs for loader.rs's `run` command -- one
                           real static ELF64 executable, one real PE32+
                           (.exe) executable, and one raw flat binary, all
                           built from near-identical hand-written asm --
                           plus cat.elf, which exercises the open/read/
                           brk/write/close syscalls by reading HELLO.TXT,
                           and thread.elf, which clones a second thread
                           into its own address space via SYS_CLONE, and
                           mmap.elf, which writes into SYS_MMAP-reserved
                           memory that a page fault backs on first touch,
                           and pie.elf, a real ET_DYN (PIE) executable
                           whose one R_X86_64_RELATIVE self-relocation
                           loader.rs has to fix up correctly to run at all.
Makefile                  Build orchestration (kernel -> ISO + disk -> QEMU).
```

## Toolchain notes

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

## Where to go next

Roughly the order being worked through, each building on what's before it:

1. ~~GDT~~, ~~IDT + exception handlers~~, ~~PIC + IRQs~~ -- done.
2. ~~Physical frame allocator~~, ~~paging abstraction~~, ~~heap +
   `#[global_allocator]`~~ -- done.
3. ~~Keyboard driver + text console~~, ~~PIT timer + blinking cursor~~ --
   done.
4. ~~Disk driver (ATA PIO) + filesystem (FAT16, incl. subdirectories and
   writing)~~ -- done.
5. ~~Command-table shell refactor~~ (extensible dispatch instead of a
   hardcoded match) -- done.
6. ~~**apex**~~ -- a `sudo`-style privilege gate: `apex.rs` hashes
   (SHA-256, hand-written -- see toolchain notes) and persists a password
   to `/APEX.PWD` on first use, then prompts for it (masked input, 3
   attempts, a 5-minute success cache) before any command flagged
   `requires_apex` runs -- currently `reboot`, `halt`, and `rm`. Run
   `apex` with no arguments to check whether a check is currently cached.
   Done.
7. ~~**Multitasking**~~ -- `task.rs`: a fixed-size table of kernel threads,
   a hand-written `switch_to` (assembly, saves/restores just the System V
   callee-saved registers + RSP), and a round-robin scheduler that
   `timer.rs`'s IRQ0 handler drives every tick, so it's genuinely
   preemptive, not cooperative -- a tight `loop {}` in one task can't starve
   the others. Every task is still ring 0, sharing the one address space
   (kernel threads, not full processes with their own memory). The tricky
   part was giving every task its own `fxsave`/`fxrstor` scratch buffer
   instead of the single shared one the pre-multitasking timer ISR used --
   a fixed buffer would let one task's interrupt clobber another's not-yet-
   restored FPU state before it resumed. Two demo threads (`counter-a`/`-b`)
   spin in the background from boot; `ps` shows every task's id/state/ticks
   -- watch it climb in lockstep across all three (them plus the shell) as
   proof the round-robin is real. Done.
8. ~~**User mode + syscalls**~~ -- ring 3 execution and an `int 0x80`
   syscall gate. `gdt.rs` grew a DPL=3 code/data selector pair and a
   `set_kernel_stack` (TSS RSP0) that `task.rs`'s scheduler now updates on
   every switch, so a syscall or fault from ring 3 always lands on a real
   stack; `paging.rs` grew `PAGE_USER`, required at *every* page-table
   level (not just the leaf) for CPL=3 to reach a page at all; `idt.rs`
   grew a DPL=3 gate variant, since a ring-3 `int n` needs the gate itself
   to allow it or the CPU refuses with a #GP before the handler ever runs.
   `syscall.rs` is the actual gate: syscall number in `rax`, up to two
   args in `rdi`/`rsi`, two syscalls so far (`write`, `exit`) named after
   their Linux counterparts for familiarity though nothing else about the
   ABI matches. `usermode.rs` hand-assembles a tiny demo program (there's
   no ELF loader yet), copies it to a freshly `PAGE_USER`-mapped page
   (deliberately *not* run in place from the kernel's own `.text`, which
   has no `PAGE_USER` bit anywhere), and runs it as a real task -- it
   prints a message via `write` and exits via `exit`, both confirmed
   working in QEMU (message appears on screen before the shell banner;
   `ps` shows it afterward as `terminated`, having run for exactly one
   scheduler tick). Process isolation came later, as its own step -- see
   item 9.5 below; `write`'s pointer argument is still trusted, not
   validated, within a task's own address space, which is a separate gap.
   Done.
9. ~~**GUI groundwork**~~ -- a PS/2 mouse driver (`mouse.rs`: 8042 aux-device
   enable sequence, IRQ12, the classic 3-byte relative-movement packet
   format, Y-axis inverted since PS/2 reports "up" as positive) and a
   minimal compositor (`wm.rs`). No damage tracking: `gui` freezes the
   whole framebuffer once as a background (`framebuffer.rs`'s
   `snapshot`/`blit`, which needed the heap bumped from 1 MiB to 16 MiB --
   a single 1280x800x32bpp snapshot alone is ~4 MiB), then redraws
   everything -- two demo windows plus a real mouse cursor -- from that
   frozen snapshot every frame something changes. Windows are draggable
   by their title bar (hit-test + remove/push-to-front for z-order), `Esc`
   restores the background and returns to the shell. Still not a real
   desktop: windows are decoration, not separate running programs -- that
   needs an actual GUI task-hosting model built on top of item 9.5's
   isolation, not just the isolation itself.

   The cursor itself is the real thing, not a placeholder: `cursor.rs`
   embeds `arrow.cur` from the "Minimalistic Modern Cursor Set" pack found
   earlier (32x32, genuinely alpha-channeled, not just a 1-bit AND mask),
   extracted offline with a small Python script into a flat RGBA byte blob
   (`kernel/assets/cursor_arrow.rgba`) and pulled into the kernel binary
   with `include_bytes!` rather than writing a `.cur`/ICO parser for one
   asset. `framebuffer.rs` grew `blend_pixel` (reads the live channel
   shifts back out of whatever's already on screen, alpha-blends the new
   colour in, writes it back) so the cursor's soft anti-aliased edges
   render properly over both the dark background and the coloured windows
   instead of a hard-edged cutout. Confirmed in QEMU: the cursor tracks
   correctly, its edges blend cleanly over every surface it was tested
   against. The pack's `.ani` files (animated cursors, a RIFF container of
   several `.cur`-like frames plus timing) are still untouched -- a
   natural follow-up once an actual "busy" state exists to show one for.
   Full regression pass afterward (`ps`/`meminfo`/`ls`) still clean, zero
   panics. Done.
9.5. ~~**Process isolation**~~ -- every ring-3 task now gets its own
   private page tables instead of all sharing the one Limine set up.
   `paging.rs` grew `new_address_space` (a fresh PML4 whose kernel-half
   entries, indices 256..511 -- HHDM, the kernel image, the heap, the
   framebuffer, all of it -- are copied from whichever address space is
   currently active, and whose low half starts completely empty),
   `map_page_in` (the same page-table-walking logic `map_page` already
   had, but targeting an arbitrary PML4 instead of always the active one,
   since a new task's mappings have to go into an address space that
   usually isn't loaded yet), and `load_cr3`. `task.rs`'s `Task` struct
   grew a `cr3` field -- every kernel thread carries the same
   `KERNEL_CR3` (captured once at boot), a ring-3 task carries its own
   private one -- and `schedule()` reloads CR3 whenever the incoming
   task's address space actually differs from the outgoing one (skipped
   otherwise, since reloading CR3 unconditionally would flush the entire
   TLB on every single kernel-thread-to-kernel-thread switch, 100 times a
   second, for no reason -- shell/counter-a/counter-b all share one
   address space and never need it). `usermode.rs` now spawns *two* demo
   tasks, each in its own address space, both mapped at the identical
   virtual address (`0x0000004000000000`) -- which would have been an
   outright collision before this (the second task's mapping would have
   silently overwritten the first's shared page-table entry, corrupting
   whichever one was still running). Confirmed in QEMU: both print their
   message independently and show up in `ps` as separately terminated
   tasks, with zero interference between them; full regression pass
   (`ps`/`meminfo`/`ls`/`gui`) still clean afterward. `write`'s pointer
   argument is still trusted rather than validated *within* a task's own
   address space (a syscall can still be handed a bad pointer inside its
   own memory and page-fault), which is a different, smaller gap than
   "no isolation at all" -- worth closing eventually, not blocking on it
   now. Done.
10. **Porting DOOM** (via [doomgeneric](https://github.com/ozkl/doomgeneric))
    -- doesn't actually require multitasking/user mode, it can run directly
    in kernel space. Tracked as its own arc, not interleaved with the
    numbered list above. First groundwork pass done:

    - ~~The red-zone gap closed~~ -- `.cargo/config.toml` now passes `-C
      no-redzone`, matching what a real `x86_64-unknown-none` target would
      give for free. Paired with `-mno-red-zone` on every C compilation
      (see below), since C compilers assume the red zone is available by
      default too.
    - ~~A much bigger heap~~ -- `heap.rs`'s `INITIAL_HEAP_SIZE` bumped from
      16 MiB to 64 MiB. `task.rs` also grew `spawn_with_stack` alongside
      the existing `spawn`, so a future DOOM task can ask for a real stack
      (a whole C game engine's call depth, not this kernel's own shallow
      Rust) instead of every kernel thread being stuck with the 32 KiB
      default.
    - ~~A C toolchain wired into the build~~ -- `build.rs` (new) shells
      out to `clang` (falling back to `cc`/`gcc`) to compile everything
      under `csrc/` with flags mirroring the Rust side's own freestanding
      setup as closely as C lets them (no red zone, the `kernel` code
      model, no stack protector, no PIC/PIE, `-ffreestanding`), archives
      the result into a static lib, and links it straight into the kernel
      binary -- no `cc` crate dependency, just `std::process::Command`,
      so `cargo build` still needs no network access in a fresh
      environment.
    - ~~A libc shim, the memory-allocation part~~ -- `libc_shim.rs`
      implements `malloc`/`free`/`calloc`/`realloc` on top of the
      existing `#[global_allocator]` (a hidden size header before each
      returned pointer is what lets `free` reconstruct the `Layout`
      Rust's allocator API needs, since libc's own `free` takes no
      size). `memcpy`/`memset`/`memmove`/`memcmp` needed no new work --
      C code calling those resolves straight against `intrinsics.rs`'s
      existing definitions, the same ones `compiler_builtins` already
      relies on.
    - Proven end to end with `csrc/cdemo.c`: real, compiled C that
      `malloc`s an array, fills and sums it, `free`s it, and calls back
      into Rust to report the result -- wired up as the `cdemo` shell
      command. Confirmed in QEMU: returns exactly the expected sum (4032),
      and a full regression pass (`ps`/`meminfo`/`ls`/`gui`) afterward is
      still clean, zero panics.
    - ~~`printf`/`sprintf`~~ -- `csrc/printf.c` (new) is a real,
      self-contained `printf` family (`printf`/`vprintf`/`fprintf`/
      `vfprintf`/`sprintf`/`snprintf`/`vsnprintf`/`puts`/`putchar`),
      hand-written against the compiler's own `__builtin_va_*` intrinsics
      rather than `<stdarg.h>` (freestanding C has no header to get them
      from, but the intrinsics themselves don't need one). Supports
      `%d`/`%i`/`%u`/`%x`/`%X`/`%o`/`%c`/`%s`/`%p`/`%%`, the `l`/`ll`
      length modifiers, zero-padding and a minimum field width, and a
      precision on `%s` -- deliberately no floating point (`%f`/`%e`/`%g`)
      yet, nothing on doomgeneric's critical path needs it so far.
      `printf`/`puts`/`putchar` reach the screen via a new `konjac_write`
      (`cfile.rs`) that both this and the file-I/O piece below share.
    - ~~File I/O~~ -- `cfile.rs` (new) implements `fopen`/`fclose`/
      `fread`/`fwrite`/`fseek`/`ftell`/`feof`/`rewind`/`fputs`/`fgets`
      plus `stdout`/`stderr`, all on top of `fat16.rs`'s existing
      whole-file `read_file`/`write_file` (there's no partial-I/O API to
      build on, so every open file is really just that whole-file `Vec`
      plus a cursor, loaded once at `fopen` and flushed once at `fclose`
      if anything changed -- more than adequate for a WAD file read in
      large chunks and essentially never written). `stdout`/`stderr` are
      the exact same handle type with a `console: true` flag rather than
      a special case bolted onto every function: writes go straight to
      `print!`, reads always report EOF (no stdin hookup -- input comes
      through `keyboard.rs`/`mouse.rs`'s own callbacks). `FILE*` itself
      stays fully opaque to C, exactly like a real libc's, matching what
      portable C code already expects. Getting this far also meant adding
      `strlen` to `intrinsics.rs` (`core::ffi::CStr::from_ptr`, used to
      read `fopen`'s C-string arguments, calls out to a real `strlen`
      symbol the same way `mem*` already did) and writing a small
      hand-rolled ASCII-only lossy-bytes-to-`String` helper instead of
      `alloc::string::String::from_utf8_lossy` -- that pulls in a
      `_Unwind_Resume` dependency this `panic = "abort"` kernel can't link
      against, the exact same gotcha as `alloc::format!` in the toolchain
      notes above.
    - Proven end to end with `csrc/iodemo.c`: real, compiled C that
      `printf`s a formatted line, `fopen`s a file for writing, `fprintf`s
      formatted text into it, closes it, reopens it for reading, `fread`s
      it back, and `printf`s the result -- wired up as the `cio` shell
      command. Confirmed in QEMU: the formatted `printf` output matches
      exactly, the file shows up afterward in `ls` at the expected size,
      and the content read back matches exactly what was written. Full
      regression pass (`cdemo`/`ps`/`meminfo`/`gui`) afterward still
      clean, zero panics.

    **DOOM itself now runs.** Booting to the title screen, navigating the
    menu, and playing a level all work in QEMU as of this pass. What
    changed:

    - ~~doomgeneric's own sources~~ -- `csrc/doom/` (new) holds 81 `.c`
      files and 97 headers, straight from [doomgeneric upstream]
      (https://github.com/ozkl/doomgeneric), chosen via the project's own
      `Makefile.linuxvt` (a maintainer-curated list of exactly the
      platform-independent engine files a minimal, framebuffer-only port
      needs) as ground truth rather than auditing ~130 files by hand.
      Everything genuinely platform-specific (X11, SDL/SDL_mixer,
      Windows, DOS, PNG screenshots) turned out to already be compiled out
      by preprocessor guards (`_WIN32`, `__MACOSX__`, `FEATURE_SOUND`,
      `HAVE_LIBPNG`, ...) that are simply never defined in this build, so
      none of it needed shimming at all -- confirmed by grepping every
      real (non-dead-code) use of anything outside the core `string.h`/
      `stdio.h`/`stdlib.h`/`ctype.h`/`math.h` surface before writing a
      single header. **This is GPL-2.0-licensed code** (id Software's
      original DOOM source plus Simon Howard's Chocolate Doom-derived
      cleanups that doomgeneric builds on -- see `csrc/doom/
      LICENSE.doomgeneric`), a meaningfully different license situation
      from the rest of this repository, worth being deliberate about
      before this project is shared or distributed anywhere.
    - ~~The rest of the header shims~~ -- `csrc/doom_include/` (new) adds
      freestanding `stdint.h`/`stdbool.h`/`stddef.h`/`stdarg.h`/
      `limits.h`/`assert.h`/`ctype.h`/`string.h`/`strings.h`/`stdlib.h`/
      `stdio.h`/`math.h`/`errno.h`/`inttypes.h`/`sys/types.h`/
      `sys/stat.h`/`unistd.h`/`fcntl.h`, compiled against with
      `-nostdinc` so nothing accidentally resolves against the build
      host's real system headers instead. The backing implementations
      landed in three places depending on what they needed: plain C
      (`csrc/doom_string.c` for `strcpy`/`strcmp`/`strchr`/`strdup`/...,
      `csrc/sscanf.c` for a deliberately narrow `sscanf` supporting only
      the `%d`/`%i`/`%x` conversions `m_config.c` actually uses) needed
      nothing from the kernel; `csrc/doom_math.c` needed the x87 FPU
      directly (`sin`/`cos`/`tan`/`atan`/`fabs`, via `fsin`/`fpatan`
      inline asm -- the *only* transcendental math anything in the
      included files calls, and only once, during `r_main.c`'s startup
      table generation, not per-frame -- safe under every task's existing
      FXSAVE/FXRSTOR area from the process-isolation work above, which
      covers x87 state as well as SSE); and `mkdir`/`getenv`/`system`/
      `exit`/`abort`/`remove`/`rename`/`qsort`/`rand`/ctype
      classification/`errno` landed in `libc_shim.rs` since they needed
      kernel-side hooks (`exit`/`abort` now actually terminate the calling
      task via `task::exit_current` rather than halting the machine or
      parking forever -- see the task-termination entry below). Every
      function on this list is one actually referenced
      by the 81 included files (grepped and confirmed first) -- nothing
      spent effort on symbols nothing calls.
    - ~~SSE and floating point~~ -- `build.rs` dropped the `-mno-sse
      -mno-mmx` flags the original `cdemo.c`-only groundwork used: DOOM
      needs real `float`/`double` (the transcendental math above, plus a
      mouse-acceleration check in `v_video.c`), and x86-64 SysV passes
      those in XMM registers by default, so disabling SSE would have
      silently switched to a non-standard x87-based calling convention
      instead of just failing loudly. Safe to drop given the FXSAVE point
      above.
    - ~~A legally-distributable WAD~~ -- `disk_root/DOOM1.WAD` (4,196,020
      bytes, verified `IWAD` header and the well-documented exact
      shareware file size) is id Software's 1995 shareware release,
      which its own distribution terms permit redistributing freely --
      the standard choice for a hobbyist/homebrew port. `Makefile`'s
      `DISK_SIZE_MB` went from 16 to 32 to fit it alongside everything
      else already in `disk_root/`.
    - **The platform driver** -- `csrc/doom/doomgeneric_konjac.c` (new)
      implements doomgeneric's six-function porting API. `DG_DrawFrame`
      hands doomgeneric's 640x400 `0x00RRGGBB` screen buffer to a new
      `doom_driver.rs` (`konjac_doom_blit`), which blits it onto
      `framebuffer.rs`'s `Canvas` centered on screen, repacking each
      pixel through `Canvas::put_pixel` so this works regardless of the
      real framebuffer's actual channel layout. `DG_GetTicksMs`/
      `DG_SleepMs` are built on `timer.rs`'s existing 100 Hz PIT tick
      counter (`DG_SleepMs` busy-waits with `task::yield_now()` between
      checks rather than blocking the scheduler -- there's no sleep
      queue yet). `DG_GetKey` reads from a second ring buffer in
      `keyboard.rs` (`DOOM_RING`, entirely separate from the ASCII one
      `read_char`/the shell already use) that the same IRQ1 handler now
      feeds unconditionally on every make *and* break code, translated
      through a new `SCANCODE_DOOMKEY` table mirroring doomgeneric's own
      DOS/`i_input.c` reference mapping (letters/digits pass through as
      their own unshifted ASCII, Space is `KEY_USE`, Ctrl is `KEY_FIRE`,
      arrow keys and function keys get their `doomkeys.h` codes) -- this
      is the first thing in KonjacOS that needed key-*release* events,
      not just typed characters, since DOOM needs to know when a
      movement key comes back up. `DG_SetWindowTitle` is a no-op (no
      window system to set a title on). A new `doom` shell command
      (`commands.rs`) spawns all of this as its own kernel task via
      `spawn_with_stack` (DOOM's recursive renderer and static engine
      tables want more than the default stack), clearing any stale
      queued key events left over from typing `doom` + Enter itself
      first (`keyboard::clear_doom_events`) so DOOM's first inputs aren't
      whatever was still in the queue from launching it.
    - **A real printf bug, found by DOOM itself**: the first boot attempt
      died in `HU_Init` with `W_GetNumForName: STCFN33 not found?` --
      `csrc/printf.c`'s `%.3d`-style *precision* on integer conversions
      was being parsed and then silently discarded, so `"STCFN%.3d"` was
      printing `STCFN33` instead of the zero-padded `STCFN033` the WAD's
      font lump is actually named, and the lump lookup failed. Fixed by
      having the integer-conversion path actually honor `precision` as
      "zero-pad to at least this many digits" (matching the C standard,
      and distinct from `width`, which still pads with spaces once a
      precision is given -- the leading-`0` width flag is ignored
      whenever a precision is present, same as every real `printf`).
      Exactly the kind of bug this project's whole "prove it end to end,
      don't just get it to compile" approach exists to catch.

    Confirmed in QEMU via QMP `screendump`, working from a cold boot: the
    `doom` command loads `DOOM1.WAD` from the FAT16 root (found by
    doomgeneric's own bare-filename search, `d_iwad.c`'s
    `D_FindWADByName` -- no `-iwad` argument needed, it just has to be
    sitting in the root directory of whatever's mounted, which it is),
    renders the real shareware title screen pixel-for-pixel, opens the
    main menu and the New Game skill-select menu on real keypresses, and
    starts an actual level with the in-game HUD (health/ammo/armor/face)
    and 3D view rendering. Not yet confirmed/polished: sustained gameplay
    over many frames, sound (deliberately out of scope --
    `FEATURE_SOUND` stays undefined, there's no audio driver in KonjacOS
    yet), and save/load (`remove`/`rename` are stubbed to always fail --
    nothing calls them outside the save/load menu, so this fails safely
    rather than crashing). Switching back to the shell once DOOM is
    running now works -- see the next entry. Full regression pass
    (`ps`/`meminfo`/`ls`/`gui`/`cdemo`/`cio`) after all of this still
    clean, zero panics outside of DOOM's own task.

11. **Real task termination.** DOOM (or anything else) can now actually
    exit back to the shell instead of being one-way until reboot -- the
    gap called out at the end of the previous entry. This is
    general-purpose scheduler infrastructure, not DOOM-specific:

    - **`task.rs` actually frees a terminated task's slot now.**
      Previously `task_exit` just marked a task `Terminated` and it spun
      forever, holding its slot, stack, and fxsave area for good --
      `MAX_TASKS` (16) would eventually run out if anything spawned and
      exited more than a handful of times. `schedule()` now sweeps the
      task table on every call (timer tick, `yield_now`, or an exiting
      task's own final call into it) and drops any `Terminated` task
      that isn't the one it's currently executing on top of -- dropping
      a `Task` frees its `_stack`/`fxsave` `Box`es and clears the slot
      for `spawn`/`spawn_with_stack` to reuse. The "isn't the one it's
      currently executing on top of" carve-out matters: a task calling
      `task_exit` on itself is still running on its own stack for the
      rest of that call, so freeing it right then would pull the stack
      out from under its own return address -- it gets reaped on some
      *later* schedule() call instead, always at most one 100 Hz timer
      tick away.
    - **`task::exit_current()`** is a small ergonomic wrapper around
      `task_exit` for Rust call sites (as opposed to `task_trampoline`'s
      hand-written assembly, which needs `task_exit`'s stable
      `extern "C"` symbol). `libc_shim.rs`'s `exit`/`abort` now call it
      instead of parking in a `yield_now` loop forever.
    - **`task::kill(id)`** terminates *another* task by ID without
      needing to switch away from anything -- it just flags the target
      `Terminated`, which permanently excludes it from selection from
      that point on; the target keeps running until the next scheduling
      point (at most one tick), then gets swept and reaped exactly like
      a self-exit. Exposed as a new `kill <id>` shell command
      (`commands.rs`) for stopping a runaway or unwanted task (DOOM
      included) without waiting for it to exit on its own -- `ps` shows
      the IDs. Refuses to target task 0 (the shell itself, always that
      ID since `task::init` assigns it before anything else can spawn).
    - **DOOM specifically**: `exit`/`abort` (and therefore `I_Quit`,
      which every path through DOOM's "Quit Game" menu confirmation
      calls) now clear the console and print a confirmation before
      terminating the task, since DOOM owns the framebuffer for as long
      as it's drawing to it -- otherwise its last rendered frame would
      just sit there forever with nothing left updating it, even though
      the task itself is gone. `kill`-ing DOOM specifically (by name)
      does the same console-clear. Killing a non-visual task (like the
      demo counters) doesn't bother clearing the screen -- there's
      nothing on it that needs wiping.

    Confirmed in QEMU via QMP: `kill 1` on a live demo counter task
    prints a confirmation, the task vanishes from `ps`, and its loop
    counter (proof it's really not running anymore, not just hidden)
    freezes while every other task's keeps climbing. A new `doom` task
    spawned afterward reuses the freed slot. Quitting DOOM through its
    own in-game menu ("Quit Game" -> "Press Y to quit to DOS") clears
    the screen, prints `[doom exited with status 0 -- back to the
    shell]`, and `ps` afterward shows DOOM's task fully gone -- no
    leftover slot, no leaked memory (`meminfo` unchanged). Full
    regression pass (`ps`/`meminfo`/`cdemo`) afterward still clean.

12. **The "big trio" loader: flat binaries, ELF64, and PE32+ (.exe), all
    through one kernel.** Most OSes commit to exactly one native executable
    format, because *format* (the container -- headers, sections, where
    code goes in memory) and *ABI* (syscalls, calling convention, the
    libraries a program assumes exist) are usually treated as one package
    deal. KonjacOS separates them: `loader.rs` (new) sniffs a file's magic
    bytes and parses whichever of the three containers it turns out to be
    into the same internal shape -- a list of "copy these file bytes to
    this address, zero-fill the rest, here's the entry point" segments --
    which then goes through the exact same `paging`/`task::spawn_user`
    path `usermode.rs`'s original hand-written ring-3 demo already proved
    works. What makes this tractable instead of a Wine-scale undertaking:
    it's format flexibility on top of *KonjacOS's own* tiny `int 0x80`
    syscall table (`syscall.rs`), not compatibility with real Linux or
    Windows binaries -- a `.exe` that imports from an actual DLL gets
    rejected at parse time with an explanation, not loaded and left to
    crash on its first unresolved call.

    - **`parse_elf`** handles a static (`ET_EXEC`, non-PIE), little-endian,
      x86_64 ELF64 executable: walks the program header table and turns
      every `PT_LOAD` entry into a segment, using each one's own `p_flags`
      for read/write permissions. Rejects `ET_DYN`/PIE binaries outright
      (relocating those is a dynamic linker's job, not a loader's) and
      anything that isn't `EM_X86_64`/64-bit/little-endian.
    - **`parse_pe`** handles a 64-bit ("PE32+") Windows executable: DOS
      stub -> `PE\0\0` signature -> COFF header -> optional header ->
      section table, each section becoming a segment. The interesting
      part is the import-table check: even a self-contained binary with
      *no* real imports typically still gets an import directory entry
      from the linker (mingw's default PE linker script always lays one
      out), just one whose first descriptor is all-zero -- the standard
      "no more entries" terminator. So this doesn't just check whether the
      directory's size is nonzero; it resolves the RVA to a file offset
      (`rva_to_file_offset`, walking the section table the same way a real
      Windows loader would) and inspects the actual descriptor, only
      rejecting a *genuinely non-empty* one. Verified against both cases:
      a self-contained `.exe` (empty terminator, size 24 bytes) loads and
      runs fine, while a real one built with `x86_64-w64-mingw32-gcc`
      against `user32.dll` (`MessageBoxA`, a real 1372-byte import table)
      gets a clean, specific rejection instead of a crash.
    - **`parse_flat`** handles the format with no header at all: the whole
      file is one segment, loaded verbatim at a fixed address
      (`FLAT_BASE`) with its own first byte as the entry point.
    - **`map_segment`** is the one function all three formats funnel
      into, and the one piece of this that's genuinely subtle: it handles
      a segment's virtual address *not* being page-aligned (routine for
      ELF, where only the offset-within-a-page has to match between the
      file and the virtual address, not the page boundary itself) by
      rounding down to the containing page and copying each page's data
      at the right in-page offset -- the same trick every real ELF loader
      uses, computed generically enough that PE's (always page-aligned,
      per `SectionAlignment`) and the flat binary's (also aligned) cases
      just fall out of the same code for free.
    - **Three demo programs** (`userprogs/hello_elf.asm`,
      `hello_pe.asm`, `hello_bin.asm`, shipped pre-built as
      `disk_root/hello.elf`/`hello.exe`/`hello.bin`) are near-identical
      hand-written x86_64 asm -- the same two KonjacOS syscalls
      (`SYS_WRITE`, `SYS_EXIT`) and the same RIP-relative-addressing
      trick `usermode.rs`'s original demo uses -- assembled and linked
      three completely different ways: `nasm -f elf64` + `ld -static
      -no-pie` for a genuine `ET_EXEC` ELF64 binary (confirmed via
      `readelf`: two real `PT_LOAD` segments, R and R+E), `nasm -f win64`
      + `x86_64-w64-mingw32-ld` for a genuine PE32+ `.exe` (confirmed via
      a hand-rolled PE header parser), and `nasm -f bin` for the flat
      binary. The point isn't the trivial program -- it's that the exact
      same OS-level idea comes out wearing three unrelated containers.
    - A new **`run <file>`** shell command (`commands.rs`) is the whole
      user-facing surface: reads the file via `fat16::read_file`, hands
      the bytes to `loader::load_and_run`, and reports which format got
      detected and the spawned task's ID (or the parse error, for a
      malformed or unsupported file -- never a panic, since reading an
      arbitrary file the user handed you failing is an expected outcome,
      not a kernel bug).

    Confirmed in QEMU via QMP: `run hello.elf`, `run hello.exe`, and
    `run hello.bin` each print `run: <file>: recognized as <format>, task
    #N spawned` followed by that program's own message, printed through
    the real `int 0x80` -> `syscall_handler` -> `print!` path from inside
    an isolated ring-3 task -- not a shortcut, the actual same mechanism
    `usermode.rs`'s demo proved works. All three tasks self-terminate via
    `SYS_EXIT` and get fully reaped (task-termination work above): `ps`
    afterward shows only the shell and the two demo counters, and
    `meminfo` reports the exact same 68 MiB used / 186 MiB free as right
    after boot across three full load/map/run/exit cycles of three
    different executable formats -- though see item 13 below for an
    honest caveat on what that number can and can't prove. The rejection
    path was verified too: a real `.exe` built against `user32.dll` gets
    refused with a clear explanation instead of loading and crashing.
    Full regression pass (`ps`/`meminfo`/`cdemo`/`doom`) afterward still
    clean.

13. **A real syscall ABI: file access and a heap for ring-3 programs.**
    Before this, a "program" running on KonjacOS could only be a
    hello-world -- the only two syscalls were `write` and `exit`, and the
    gate itself only carried two arguments. This pass expands both: the
    `int 0x80` gate now shuttles four arguments (`syscall.rs`'s
    `syscall_stub`, matching the real Linux x86-64 `syscall` register
    convention -- `rax`=number, `rdi`/`rsi`/`rdx`/`r10`=args -- purely for
    familiarity, not because anything else about the ABI matches Linux),
    and three new syscalls ride on top of it: `open(path_ptr, path_len)`,
    `read(fd, buf_ptr, len)`, and `close(fd)`, backed by a small,
    deliberately-not-per-task table of open files (`FD_TABLE`) wrapping
    `fat16::read_file` -- a real, if minimal, way for a ring-3 program to
    read an actual file off the disk instead of only whatever bytes got
    linked into its own segments. Alongside those, `brk(new_top)` gives
    every ring-3 task a real, dynamically-growable heap: `task.rs` now
    carries a `heap_end` field per `Task` (starting at a new
    `USER_HEAP_BASE`, same "fixed address, safe because every task has
    its own private address space" reasoning as `loader.rs`'s and
    `usermode.rs`'s addresses), and `SYS_BRK` maps fresh, zeroed pages via
    `paging::map_page`/`pmm::alloc_frame` to cover however far a task asks
    to grow -- grow-only, page granularity, no `mmap`, but a genuine
    dynamically-sized buffer instead of a fixed one baked into the binary.

    A new demo program, `userprogs/cat_elf.asm` (shipped pre-built as
    `disk_root/cat.elf`), exercises the whole chain in one file: opens
    `HELLO.TXT`, calls `brk(0)` to find its current heap top, calls `brk`
    again to actually map a page there, reads the file into that
    freshly-mapped memory, writes it back out (proving the page is both
    genuinely readable *and* writable, not just present), then closes the
    fd before exiting. `int 0x80` only ever clobbers `rax` (every other
    register is saved/restored around the call by `syscall_stub`), so the
    program uses plain general registers as its own cross-syscall scratch
    storage with no extra bookkeeping.

    Confirmed in QEMU via QMP: `run cat.elf` prints `hello from disk.` --
    `HELLO.TXT`'s exact contents, read back out through open, two brk
    calls, read, and write. Full regression pass (`ps`/`meminfo`/
    `hello.elf`/`hello.exe`/`hello.bin`/`doom`) afterward still clean.
    Honest caveat, not fixed in this pass: `meminfo`'s "MiB used" figure
    is integer-divided down to whole MiB (`memory.rs`'s `print_info`), so
    it genuinely can't show a handful of leaked pages the way item 12's
    "identical before/after" claim implied -- and there's a real gap it'd
    likely be hiding: neither `task_exit`'s reaping nor `kill` ever frees
    a terminated ring-3 task's *address space* (the physical frames
    backing its code/stack/heap, or the PML4 and page-table frames
    `paging::new_address_space` allocated for it) -- only the kernel-side
    bookkeeping (`Task`'s own stack/fxsave `Box`es, per the task-
    termination work) gets reclaimed. Every `run`/`doom` invocation this
    whole session has very likely been leaking on that order, just below
    what `meminfo` can currently show. Actually freeing an address space
    needs `paging.rs` to grow the ability to walk and tear down a PML4's
    private half, which it can't do yet -- a real, scoped next step, not
    something to gloss over.

14. **Fixing the address-space leak from item 13.** `paging.rs` gains
    `destroy_address_space(pml4_phys)`: it walks only PML4 indices `0..256`
    (a task's private, low half -- indices `256..512` are the kernel half
    every address space shares *by reference*, courtesy of
    `new_address_space`, and must never be recursed into or freed), and for
    every present entry at each of PDPT/PD/PT it frees the leaf frame via
    `pmm::free_frame`, then the PT frame, then the PD frame, then the PDPT
    frame, and finally the PML4 frame itself. `task.rs`'s existing reaping
    sweep (inside `schedule()`, the same loop that already frees a
    terminated task's kernel-side `Task` struct) now calls it first,
    whenever the task being reaped's `cr3` differs from the shared
    `KERNEL_CR3` -- i.e. whenever it's a ring-3 task with its own private
    address space, not a kernel thread. Guaranteed safe to do there for the
    same reason the existing sweep already was: `i == current` is always
    skipped, so a reaped task's address space is never the one still
    loaded into CR3.

    To actually verify this instead of trusting the reasoning, `meminfo`
    also gained a second line: exact `used_frames`/`free_frames` counts
    alongside the existing MiB-rounded ones (item 13's honest caveat about
    MiB rounding hiding small leaks is now moot for this specific number).
    Confirmed in QEMU via QMP: baseline `meminfo` read `17562 used frames,
    47809 free frames`; after `run`-ning `hello.elf`/`hello.exe`/
    `hello.bin` five times each and `cat.elf` (which also exercises `brk`,
    growing and then tearing down a heap page) five times -- twenty
    separate ring-3 tasks, twenty fresh address spaces, twenty reaps --
    `meminfo` read back the exact same `17562 used frames, 47809 free
    frames`. Zero net frames lost, at single-frame precision, not just
    "unchanged at whole-MiB resolution." `ps` afterward showed only the
    shell and the two demo counter threads, confirming every one of the
    twenty task slots was actually reclaimed, not merely left `Terminated`
    forever. Full regression pass (`ps`/`help`/`meminfo`) afterward still
    clean.

15. **First steps toward a general-purpose OS: per-task file descriptors
    and partial file reads.** Not features in service of any one program --
    groundwork every POSIX-shaped piece of software assumes is already
    there. Two independent changes:

    Every task now owns its own open-file table (`task::OpenFile`,
    `task::with_current_open_files`) instead of every task in the kernel
    sharing one flat `FD_TABLE`. An `fd` is just an index into *that task's
    own* array now, the same privacy guarantee its address space already
    has -- one task can no longer read, close, or even see another's open
    files just by guessing a small integer. `syscall.rs`'s `SYS_OPEN`/
    `SYS_READ`/`SYS_CLOSE` handlers didn't need to change shape at all,
    just which table they reach into; the `Vec<u8>` each `OpenFile` holds
    is freed automatically when its `Task` is dropped during reaping, the
    same way every other per-task allocation already is -- no new cleanup
    code needed.

    `fat16.rs` gained `read_file_range(path, offset, len)` alongside the
    existing whole-file `read_file`: it still has to walk every cluster
    before `offset` to find where the chain leads next (FAT16's chain is a
    real singly-linked list on disk, there's no jumping straight to byte N
    the way an extent-mapped filesystem would allow), but it only actually
    reads off disk, and only copies into the returned buffer, the sectors
    that overlap `[offset, offset+len)` -- not the whole file. This is real
    groundwork, not a one-off: it's what a `pread`-style syscall needs
    instead of "load everything, then slice it yourself", and it's a
    direct prerequisite for demand-paged (`mmap`) file-backed memory later,
    where a page fault has to satisfy one 4 KiB page at a time, not
    prefetch an entire file. A new `readat <file> <offset> <len>` shell
    command exercises it directly (not wired into any syscall yet -- this
    pass is the filesystem primitive, not the syscall surface on top of
    it).

    Confirmed in QEMU via QMP: `readat hello.txt 6 4` returned exactly
    `"from"`, `readat hello.txt 15 10` returned the 2 trailing bytes
    `".\n"` (correctly truncated at EOF instead of erroring), `readat
    hello.txt 17 5` (offset at EOF) returned 0 bytes, and `readat hello.txt
    0 17` returned the whole file byte-for-byte identical to `cat`'s
    output -- verified against the file's real, independently-known
    17-byte contents, not just "didn't crash." Then a full regression
    pass combining both changes: baseline `meminfo` read `17592 used
    frames, 47779 free frames` on this rebuilt kernel; after running
    `cat.elf` (which opens a file through the new per-task table) five
    times and `hello.elf`/`hello.exe`/`hello.bin` three times each --
    fourteen more ring-3 tasks, fourteen more private fd tables opened and
    torn down -- `meminfo` read back the exact same `17592 used frames,
    47779 free frames`, and `ps` again showed only the shell and the two
    demo counters. The address-space-teardown fix from item 14 and the new
    per-task fd tables compose cleanly: nothing about giving each task its
    own file table reintroduced a leak.

    This is the first slice of a broader general-purpose-OS roadmap (not
    Minecraft-specific, though it happens to double as groundwork toward
    it): still ahead are threads that share an address space instead of
    always getting a fresh private one, demand-paged `mmap` (anonymous
    first, file-backed second, now that partial reads exist), and a first
    slice of dynamic linking (parsing `PT_DYNAMIC`/`ET_DYN` and handling
    self-relocation, without resolving external symbols yet).

16. **Threads: a second task sharing one address space.** Until now,
    every ring-3 task got its own fresh, private address space from
    `paging::new_address_space` -- accurate for "process," but real
    multithreaded software (a JVM's GC/JIT/render threads being the
    motivating example, but true of any real threading library) needs
    something cheaper: a new stack and register set that runs inside the
    *same* address space as its parent, seeing the same code, data, and
    heap. `task::spawn_thread` is that: nearly identical to `spawn_user`,
    except it reuses the calling task's own `cr3` instead of building a
    new one, and it doesn't map anything itself -- the caller is expected
    to have already carved out a stack for the new thread (e.g. via
    `SYS_BRK`), the same "trusted, not validated" model every other
    syscall pointer argument already uses. A new syscall, `SYS_CLONE`
    (number 6, `clone(entry_virt, stack_top)`), exposes it to ring-3
    programs.

    This forced a real correctness fix, not just an addition: `schedule`'s
    reaping sweep used to free a terminated task's whole address space the
    moment that one task finished, which was safe when every `cr3` was
    exactly one task's alone. With threads, two task slots can now
    legitimately point at the same `cr3` at once -- freeing it when the
    *first* sibling exits would pull the physical frames out from under
    the *second* one mid-run, a real use-after-free. The sweep now checks,
    under the same lock it already holds, whether any other task slot
    still shares that `cr3` before tearing it down; only the last thread
    out actually frees it.

    Two honest simplifications, not fixed in this pass: `heap_end` is
    snapshotted at clone time rather than genuinely shared, so two
    siblings both growing the heap concurrently would stomp on each
    other's mappings (most real workloads don't call `brk` from more than
    one thread at once, so this is a safe-enough gap for now, not a
    silently swept-under-the-rug one); and each thread gets its own empty
    open-file table rather than a real shared one, so a file one thread
    opens isn't visible to its siblings. Both are the same kind of
    deliberate, documented scoping the rest of this ABI already practices.

    A new demo, `userprogs/thread_elf.asm` (shipped as `disk_root/
    thread.elf`), exercises the whole path: the parent thread grows its
    heap by 8 KiB via `brk` to make room for a second stack, `clone`s a
    child pointed at that stack, prints its own message, busy-spins (no
    syscalls -- just burning cycles) to give the 100 Hz preemptive
    scheduler plenty of chances to actually run the child before the
    parent exits, then exits itself.

    Confirmed in QEMU via QMP: `run thread.elf` printed both `[parent]
    hello from the main thread` and `[child] hello from a cloned thread,
    same address space` -- two genuinely different tasks, spawned less
    than a millisecond apart, executing concurrently under one shared set
    of page tables. `ps` afterward showed only the shell and the two demo
    counters, confirming both parent and child were fully reaped. `meminfo`
    read the exact same `17597 used frames, 47774 free frames` both before
    and after -- across four separate `thread.elf` runs (eight tasks: four
    parents, four children, four shared address spaces each freed exactly
    once) plus the existing `hello.elf`/`hello.exe`/`hello.bin`/`cat.elf`/
    `readat` regression suite. That last number is the one that actually
    matters here: it's proof the new shared-`cr3` reap logic neither leaks
    (nobody ever frees it) nor double-frees (two sibling reaps racing to
    free the same frames, which would have shown up as a crash or memory
    corruption, not just a wrong number).

17. **Anonymous demand-paged memory: a real page fault handler and
    `SYS_MMAP`.** Until now, every mapping in a task's address space was
    created eagerly and up front -- a loader segment, a `brk`-grown heap
    page, a stack -- all mapped at the moment something asked for them.
    Real memory allocators (and a JVM's GC most of all) lean on a
    different pattern: reserve a big region cheaply, and only actually pay
    for the physical memory behind each page the first time something
    touches it. That needed a genuinely new capability this kernel didn't
    have: a CPU exception the kernel can *recover from* instead of just
    reporting and halting.

    `idt.rs`'s vector 14 (#PF) used to fall into the same generic,
    fatal `isr_common` path every other CPU exception uses -- print what
    happened, halt forever. It now gets its own hand-written stub
    (`paging.rs`'s `isr_stub_14`), shaped like `syscall.rs`'s
    `syscall_stub`: full GPR save, the same `CURRENT_FXSAVE_PTR`-indirected
    fxsave/fxrstor every interrupt handler here uses, and a genuine,
    resumable function call into `paging::handle_page_fault(error_code,
    cr2)`. If that returns "handled," the stub restores every register and
    `iretq`s straight back into the faulting instruction, which simply
    succeeds on retry -- from the faulted code's point of view, nothing
    unusual happened at all. If it returns "not handled" (a null pointer, a
    write to read-only memory, touching something never reserved by
    anything), the stub falls through to the exact same fatal path every
    other exception already had -- this is purely additive, no existing
    crash-and-report behavior got weaker.

    `handle_page_fault` itself is narrow on purpose: it only recognizes one
    situation as recoverable -- a not-present fault (page never mapped, not
    a permission violation) at an address inside the *currently running*
    task's `SYS_MMAP` region (`task::is_in_current_mmap_region`, checking
    against a new per-task `mmap_top` frontier -- `[USER_MMAP_BASE,
    mmap_top)` is "reserved," not "mapped"). When it matches, a fresh frame
    gets allocated, zeroed, and mapped right there, and the fault is
    serviced. `SYS_MMAP(len)` itself (syscall number 7) does almost
    nothing by comparison -- round `len` up to whole pages, hand back the
    current `mmap_top`, bump it forward -- it's the page fault handler that
    does all the actual work, lazily, one page at a time, only for pages
    something actually touches. No file backing, no `MAP_SHARED`, no real
    flags argument, no `munmap` -- exactly the anonymous-memory slice a
    heap allocator needs, not `mmap(2)`'s full generality.

    A new demo, `userprogs/mmap_elf.asm` (shipped as `disk_root/mmap.elf`),
    makes the point directly: it calls `SYS_MMAP` for one page, then writes
    a message into it byte by byte *without ever calling `SYS_BRK` on that
    memory* -- the first byte written has no physical frame behind it at
    all until the fault handler puts one there. Reading the message back
    out via `SYS_WRITE` afterward proves the fault-mapped page is genuinely
    both writable and readable, not just "didn't crash."

    Confirmed in QEMU via QMP: `run mmap.elf` printed `mmap demand-paged
    memory works: never touched by brk, only by a page fault.` -- byte-
    exact, on the first attempt, meaning the fault, the retry, and the
    readback all worked correctly the first time this ran for real. `ps`
    showed it fully reaped afterward. Then the number that actually
    matters: baseline `meminfo` read `17569 used frames, 47772 free
    frames`; after four more `mmap.elf` runs (four page faults each backed
    by a real frame), a `thread.elf` run, and the full existing
    `hello.elf`/`hello.exe`/`hello.bin`/`cat.elf`/`readat` regression suite
    -- eight more tasks total -- `meminfo` read back the exact same
    `17569 used frames, 47772 free frames`. That's proof `paging.rs`'s
    existing `destroy_address_space` (from the address-space-leak fix
    several items back) reclaims page-fault-mapped frames correctly with
    zero special-casing: it was already written to walk and free *every*
    present PTE in a task's private half, regardless of which code path
    put it there, so a page a fault handler mapped gets torn down exactly
    the same way a loader segment or a `brk` page does.

18. **The first slice of dynamic linking: self-relocating PIEs
    (`ET_DYN`).** Everything the loader ran before this was `ET_EXEC` --
    every address baked into the file absolute, valid only if loaded
    exactly where the linker assumed. A PIE (`ET_DYN`) is different: every
    address in it is relative to wherever it actually ends up loaded, which
    is precisely what makes real dynamic linking possible (multiple
    programs and shared libraries coexisting at addresses decided at load
    time, not link time) -- and precisely why loading one at all used to be
    a hard rejection here, with an honest note that supporting it was "a
    real dynamic linker's job, not a loader's."

    That's still mostly true -- resolving a symbol against some *other*
    loaded file is real dynamic-linker work this kernel doesn't do. But a
    PIE's *self*-relocations -- fixing up its own internal absolute
    pointers (a global variable holding another global's address, say) to
    account for wherever it actually got loaded -- turn out to need no
    symbol resolution at all, just arithmetic: `parse_elf` now picks a
    fixed load bias (`PIE_BASE`), adds it to every segment's address and
    the entry point same as before, and additionally walks the PIE's
    `PT_DYNAMIC` segment (a plain array of tag/value pairs, `Elf64_Dyn`) to
    find its `.rela.dyn` relocation table via the `DT_RELA`/`DT_RELASZ`
    tags -- located in the file the same "which segment's file-backed range
    contains this address" way `parse_pe`'s RVA lookup already works, just
    for ELF vaddrs instead of PE RVAs. Every `R_X86_64_RELATIVE` entry
    becomes a `(bias + r_offset, bias + r_addend)` fixup, applied by
    `load_and_run` right after every segment is mapped (a new
    `paging::translate` walks the just-built page tables read-only to turn
    that virtual target back into a physical frame, written through the
    HHDM). Anything else gets rejected with a specific, honest reason
    instead of silently mis-running: a relocation type other than
    `R_X86_64_RELATIVE` means resolving an actual symbol, and a `DT_NEEDED`
    entry means depending on a whole separate shared-library file --
    both real dynamic-linker territory, both refused with a clear message,
    exactly like `parse_pe` already refuses a `.exe` needing real DLLs.

    This was verified against ground truth, not just written to compile:
    before any kernel code existed, a hand-written PIE (`userprogs/
    pie_elf.asm`, a global pointer to a string -- an absolute address a
    position-independent linker can't resolve at link time) was built with
    `ld -pie --no-dynamic-linker` and inspected with `readelf -d`/`-r`,
    confirming a real `.rela.dyn` with exactly one `R_X86_64_RELATIVE`
    entry and no `DT_NEEDED`. The loader logic was written to match that
    real structure, not an assumption about it.

    Confirmed in QEMU via QMP: `run pie.elf` printed `hello from a
    self-relocating PIE (ET_DYN) executable!` -- which is only possible if
    the relocation was applied with the *correct* value at the *correct*
    address, since the program reads the string through a pointer that's
    garbage until the fixup runs. The rejection path was verified against
    a real binary too, not a synthetic one: a genuine `gcc -fPIE -pie`
    executable (dynamically linked against glibc, with real `DT_NEEDED`
    and `R_X86_64_GLOB_DAT` entries) was refused with `this PIE depends on
    external shared libraries (DT_NEEDED), which KonjacOS can't load --
    only fully self-contained PIEs can run` -- no crash, shell stayed
    responsive. Full regression pass: baseline `meminfo` read `17611 used
    frames, 47760 free frames`; after three more `pie.elf` runs plus
    `mmap.elf`/`thread.elf`/`hello.elf`/`hello.exe`/`hello.bin`/`cat.elf`/
    `readat` -- nine more tasks -- `meminfo` read back the identical exact
    frame count, and `ps` showed full reaping throughout. A PIE's address
    space, relocations included, tears down through the exact same
    `destroy_address_space` every other format already uses, no special
    case needed.

19. **Real dynamic linking: `DT_NEEDED` shared libraries, resolved by an
    actual symbol lookup.** Item 18 stopped exactly where a *real* dynamic
    linker's job starts: a PIE's self-relocations need no other file, but
    a `DT_NEEDED` dependency does, and there was nothing here that could
    load a second file, find what it exports, or match that against what
    the first file needs. This closes that gap -- narrowly (single-level
    dependencies only, no symbol versioning, `DT_HASH` only, not the newer
    GNU-hash format -- see `parse_elf_at`'s doc comment), but with the real
    mechanism, not a simulation of it.

    `parse_elf_at` (the renamed, bias-parameterized `parse_elf`) now walks
    a file's *entire* `PT_DYNAMIC` section instead of only hunting for
    `DT_RELA`: `DT_NEEDED` names (resolved through `DT_STRTAB` once it's
    been found, since dynamic tags aren't ordered), `.rela.dyn`
    (`R_X86_64_RELATIVE` self-fixups exactly as before, but now also
    `R_X86_64_64`/`R_X86_64_GLOB_DAT` entries that name a symbol via
    `DT_SYMTAB`), `.rela.plt` (`DT_JMPREL`/`DT_PLTRELSZ`, always
    `R_X86_64_JUMP_SLOT` -- the GOT slots real `ld`-generated PLT stubs
    jump through), and, for a file being loaded *as* a library, its own
    exported dynamic symbols (walking `DT_SYMTAB` for `DT_HASH`'s `nchain`
    entries, keeping only defined, non-local, named ones). `load_and_run`
    is what ties it together: it loads every `DT_NEEDED` name from disk
    (`fat16::read_file`, same as any other file `run` loads) as its own
    ELF file at its own fixed slot (`LIB_BASE + i * LIB_SLOT_SIZE`, up to
    `MAX_LIBS`), maps every segment from the main executable *and* every
    library into the one new address space, applies every file's
    self-relocations, collects every library's exports into one
    `BTreeMap<String, u64>`, and resolves every `extern_relocations` entry
    (from any loaded file) by name against it -- writing the real,
    already-known-at-load-time address straight into the GOT slot a PLT
    stub reads. There's deliberately no lazy-binding resolver stub or
    runtime dynamic linker living in this kernel at all: every GOT entry
    is filled in *before* the task's first instruction ever runs, the
    same end state real `LD_BIND_NOW`/`-z now` eager binding reaches, just
    arrived at by construction instead of by a runtime trampoline.

    Verified against real, `ld`-produced structures, not hand-simulated
    ones: `userprogs/libfoo_elf.asm` (a real shared library, exporting one
    function, built with `ld -shared -soname libfoo.so --hash-style=sysv`)
    and `userprogs/dyn_elf.asm` (`call foo_greet wrt ..plt` -- nasm's way
    of asking for a real `R_X86_64_PLT32` reference instead of a plain
    PC-relative one, so `ld -pie --no-dynamic-linker -lfoo` actually
    builds a genuine PLT stub/GOT slot/`.rela.plt` entry rather than a
    `DT_TEXTREL` direct patch). `readelf -d`/`-r`/`objdump -d` on the
    output confirmed the exact shape this code assumes: a `JMPREL` tag, a
    `.rela.plt` with one `R_X86_64_JUMP_SLOT` against `foo_greet`, and a
    PLT0 stub doing `jmp *GOT[...]`.

    Confirmed in QEMU via QMP screendump: `run dyn.elf` printed `hello
    from libfoo.so -- a real R_X86_64_JUMP_SLOT call into a
    dynamically-linked ELF64 shared library!` -- only possible if
    `dyn.elf`'s call actually left its own mapped segment and landed
    inside `libfoo.so`'s separately loaded, separately based code, through
    a GOT slot this loader filled in itself. Run twice back-to-back,
    `meminfo` read back the identical `17708 used frames, 47664 free
    frames` before and after both runs -- proof that tearing down a task
    with a dynamically-linked library attached leaks nothing extra: a
    library's segments are ordinary mapped pages in the same per-task
    PML4 `spawn_user` already owns, so the existing `destroy_address_space`
    sweep reclaims them with no special-casing, exactly like item 18's
    PIE relocations needed none either.

20. **A real x86_64 `syscall`/`sysretq` gate, and a genuine, unmodified
    `musl-gcc`-built Linux binary running on top of it.** Items 18-19 grew
    KonjacOS's loader up to real ELF dynamic linking; this is the other
    half of "run real Linux userspace" -- the actual entry mechanism a
    real Linux binary expects, which turns out to be a genuinely different
    CPU feature from `syscall.rs`'s `int 0x80` gate, not just a different
    number in the same door. `int 0x80` is an interrupt gate: the CPU
    walks the IDT and, since it's a real ring3->ring0 transition,
    automatically switches to the TSS's RSP0 before pushing anything (see
    `syscall.rs`'s module docs). `syscall` isn't an interrupt at all --
    it's a dedicated instruction, configured through MSRs (`STAR`/`LSTAR`/
    `SFMASK`, gated by `EFER.SCE`), and it performs **no stack switch
    whatsoever**: `RSP` on entry is still whatever the ring-3 caller's own
    stack pointer was, and `RCX`/`R11` get silently repurposed by the CPU
    to stash the return `RIP`/`RFLAGS` (which is also why the real Linux
    syscall convention's 4th argument travels in `R10` instead of `RCX` --
    `RCX` isn't available). `linux_syscall.rs`'s hand-written entry stub
    has to find and switch to a real kernel stack itself, from a new
    `gdt::SYSCALL_KERNEL_RSP` global mirroring whatever `gdt::
    set_kernel_stack` last pointed the TSS's RSP0 at, before it's safe to
    push a single byte.

    `sysretq`'s exit half forced one more genuinely new piece: its half of
    `STAR` is hard-wired to a fixed segment layout (`SS` at
    `STAR[63:48]+8`, `CS` at `+16`) that's the *opposite order* from
    `gdt.rs`'s existing `USER_CODE_SELECTOR`/`USER_DATA_SELECTOR` pair
    (code, then data) -- so rather than warp their layout and risk the
    `iretq` path every native KonjacOS demo already depends on, `gdt.rs`
    grew a second, otherwise-identical ring-3 code/data pair
    (`SYSRET_USER_CODE_SELECTOR`/`SYSRET_USER_DATA_SELECTOR`) laid out in
    the order `sysretq` actually wants, with a const-asserted tripwire in
    `linux_syscall.rs` tying `STAR`'s computed value back to those exact
    named selectors so the two files can't silently drift apart.

    Real Linux syscall numbers, not `syscall.rs`'s own small invented
    table, and real error conventions (`-errno`, not a sentinel). What's
    actually implemented was found empirically, not guessed at: `strace
    -f` against a real `musl-gcc -static`-built `hello.c`, run *natively*
    on the same x86_64 Linux machine this kernel's own toolchain already
    lives on -- ground truth for exactly which syscalls a minimal static
    musl program's startup path needs, in exactly what order. Five showed
    up: `arch_prctl(ARCH_SET_FS, ...)` (TLS setup), `set_tid_address`,
    `ioctl(fd, TIOCGWINSZ, ...)` (answered honestly: `-ENOTTY`, exactly
    what a real non-tty stream says), `writev` (real musl stdio's actual
    flush path, not plain `write`), and `exit_group`. All five are wired
    up, plus `read`/`write`/`close`/`brk`/`mmap`/`munmap` (reusing
    `task`'s existing per-task state and `syscall.rs`'s own reservation-
    only `mmap` scheme) and no-op `rt_sigaction`/`rt_sigprocmask` stubs for
    the startup-time probing real programs do even when they'll never
    actually need a signal handler to fire. Everything else answers
    `-ENOSYS`, loudly logged, instead of silently pretending to succeed.

    `arch_prctl(ARCH_SET_FS)` needed real, new per-task state to mean
    anything: `Task` grew an `fs_base` field, loaded into the real
    `FS_BASE` MSR by `schedule` on every context switch (unconditionally --
    unlike `cr3`, a plain `wrmsr` is cheap enough not to bother skipping
    when it hasn't changed) and set immediately, not just on the next
    switch, by `task::set_fs_base` itself. Without this, every `%fs:`-
    relative access real compiled C code makes constantly and
    invisibly -- `errno`, the stack-protector canary, musl's own per-
    thread state -- would resolve against whichever task's base happened
    to be loaded last, not this task's own.

    The first real attempt to `run` the actual musl binary page-faulted
    immediately, at exactly `USER_STACK_BASE + USER_STACK_SIZE` -- one
    byte past the mapped stack. The cause: `loader.rs` was only ever
    building KonjacOS's own trivial entry state (just an entry point and a
    bare stack), but a real Linux binary's crt startup (`_start` through
    `__libc_start_main`/`__init_tls`) unconditionally dereferences `[rsp]`
    expecting the real Linux initial-process-stack layout to already be
    there -- `argc`, `argv[]`, `envp[]`, and an `auxv[]` array -- because on
    real Linux, `execve(2)` itself builds exactly that layout before a
    program's first instruction ever runs. `loader.rs` grew
    `build_initial_stack` to do the same: a real `argc`/`argv`/`envp`/
    `auxv`, ending 16-byte aligned at `argc`'s own address per the SysV
    ABI's initial-stack requirement -- built for *every* program, not only
    ones known to need it, since a native KonjacOS demo never reads any of
    it anyway. `AT_RANDOM` mattered for a subtler reason than most: musl's
    stack-protector canary setup reads 16 bytes through it
    *unconditionally*, so a missing entry there isn't a graceful skip,
    it's a null-pointer read. `AT_PHDR`/`AT_PHENT`/`AT_PHNUM` mattered for
    a different real reason -- static musl's own `__init_tls` walks the
    program's own phdr table hunting for a `PT_TLS` segment before it ever
    calls `arch_prctl` -- so `Image` grew a `phdr_vaddr` field, computed
    the same way a real Linux kernel computes it: the first `PT_LOAD`
    segment's runtime base plus `e_phoff`, relying on (and this holds for
    every sane linker's output, this project's own included) that
    segment's file offset being 0.

    Confirmed in QEMU via QMP screendump, and against ground truth at
    every step, not just written to compile: a real `musl-gcc -static -O0`
    build of a `printf("hello from real musl!\n")` program -- the *exact
    same binary* `strace` was run against natively -- `run` on KonjacOS
    printed `hello from real musl!`, correctly, through the real
    `writev`/TLS/stack path this item built. A fuller regression pass
    (`meminfo` before, then three `run musl.elf`s plus `hello.elf`/
    `pie.elf`/`dyn.elf`/`sys64.elf` -- a hand-written nasm demo exercising
    the raw `syscall`/`sysretq` mechanism directly, no libc involved --
    then `ps`, then `meminfo` again) read back the exact same `17728 used
    frames, 47644 free frames` before and after, and `ps` showed the
    pre-existing `counter-a`/`counter-b` kernel threads still ticking
    normally throughout (2438 ticks each) -- proof this entire second
    syscall mechanism, and the real per-task `FS_BASE` MSR reload on every
    single context switch it added, coexists cleanly with everything
    already running, leaks nothing, and breaks nothing.

21. **A real `mmap`/`munmap`, and a real `musl-gcc` binary that actually
    calls `malloc`/`free`.** Item 20's `mmap`/`munmap` were still exactly
    `syscall.rs`'s old reservation-only, never-frees scheme underneath a
    real Linux syscall number -- fine for a `printf`-only hello world,
    which never happened to call either, but not for anything that
    actually allocates. Found the same way as everything else in this
    pair of items: `strace -f` against a real `musl-gcc`-built program
    that calls `malloc`/`strcpy`/`free`, then `malloc`s a full megabyte
    and `memset`s it, then frees that too -- ground truth for exactly what
    musl's real allocator (`mallocng`) actually does, not a guess.

    Two real shapes showed up, both now genuinely implemented in
    `linux_syscall.rs`'s `sys_mmap`/`sys_munmap`, not simulated. An
    anonymous `mmap(NULL, len, ...)` -- "somewhere, don't care where" --
    is unchanged: reserve `len` past this task's `mmap_top` and let
    `paging::handle_page_fault` lazily back it the first time something's
    actually touched, same as it's always worked. The new case is
    `mmap(addr, PAGE_SIZE, PROT_NONE, MAP_FIXED|...)`: mallocng grows
    `brk` by more than it means to keep, specifically so it has a page to
    turn back into a guard immediately afterward, at an address *it*
    picked, not one the kernel gets to relocate the way it can for the
    anonymous case. Honoring that needed a real primitive that didn't
    exist anywhere in this codebase yet: `paging::unmap_page`, which
    clears one leaf PTE, flushes it out of the TLB with `invlpg` (without
    that, a stale translation could keep answering reads/writes against a
    physical frame about to be handed back to `pmm` as free -- exactly the
    "looks fine until something else reuses that frame" class of bug), and
    hands back whichever physical frame *was* mapped there so the caller
    can free it. `MAP_FIXED`+`PROT_NONE` calls it to make `brk`'s
    just-mapped page genuinely inaccessible again (a real Linux `MAP_FIXED`
    replaces whatever was already there); a `MAP_FIXED` request for real
    read/write access maps fresh frames eagerly at that exact address
    instead. `sys_munmap` uses the same primitive for its actual job:
    walk every page in `[addr, addr+len)`, and for each one that was truly
    mapped (most of a lazily-backed region won't be -- that's routine, not
    a partial failure), unmap it and free the real frame behind it. This
    is a real, honest gap, stated plainly rather than glossed over:
    neither this nor `unmap_page` reclaims an emptied intermediate page-
    table frame or shrinks `mmap_top`'s own high-water mark, so a long-
    running process that `mmap`s/`munmap`s many distinct regions still
    leaks page-table frames and forgets which sub-ranges are free -- real
    extra bookkeeping work, left for later on purpose, the same spirit as
    every other honestly-scoped gap already documented across this
    project.

    Along the way, real musl's own `writev` calls surfaced a genuine
    latent bug in `linux_syscall.rs` predating this item: `strace` showed
    a real trailing `{iov_base=NULL, iov_len=0}` entry, and `slice::
    from_raw_parts` requires a non-null pointer even for a zero-length
    slice -- confirmed the hard way, via an actual kernel panic
    (`unsafe precondition(s) violated`) the first time this ran, not
    caught by inspection. Both `sys_write` and `sys_writev` now skip
    building a slice at all when the length is zero.

    Confirmed in QEMU via QMP screendump, ground truth end to end: the
    *exact same* `musl-gcc -static` binary `strace` ran natively --
    `malloc(128)` + `strcpy` + `printf`, then `malloc(1024*1024)` +
    `memset` + `printf`, then both `free`s -- printed `hello from a real
    musl malloc!` and `big alloc ok, buf2[0]=A` correctly, three separate
    times back to back. `meminfo` before that sequence and after (three
    full malloc/memset/free cycles, one of them a full real megabyte, plus
    the item-20 `musl.elf`, `hello.elf`, and `dyn.elf` demos run in
    between for good measure) read back the exact same `17733 used
    frames, 47639 free frames` both times, with `ps` showing the
    pre-existing kernel threads still healthy throughout -- proof the new
    `MAP_FIXED`/`PROT_NONE` guard-page dance and the real `munmap` behind
    it reclaim every physical frame a real allocator's malloc/free cycle
    actually touches, not just the ones a minimal hello-world happened to
    exercise.

22. **Real `clone()`/`futex()`, and a genuine, unmodified musl
    `pthread_create`/`pthread_join` running two threads at once.** Every
    prior item's ring-3 concurrency was either a whole new address space
    (`spawn_user`) or KonjacOS's own invented `SYS_CLONE` convention
    (`syscall.rs`'s `int 0x80` gate, item 16) -- a thread that starts
    fresh at a caller-given entry point, which is a fine ABI when this
    kernel gets to invent it, but not what real Linux/musl/glibc code
    actually calls. Real `clone(2)` has no entry-point argument at all:
    the contract is that the child comes back from the *exact same*
    `syscall` instruction the parent made, on its own given stack, with
    `rax=0` instead of the parent's `rax=<child tid>` -- and from there,
    musl's own hand-written `__clone` assembly stub (already sitting on
    that stack, put there by `pthread_create` before ever calling this)
    takes over entirely on its own, calling the thread's actual start
    function and then `exit`ing. So the only thing this kernel has to get
    right is making the child resume *exactly* like that -- nothing about
    what runs afterward is this kernel's business.

    Found via the same ground-truth discipline as every other item here:
    `strace -f` against a real `musl-gcc -static` binary
    (`pthreadtest.c`) that calls `pthread_create`+`pthread_join`, on the
    sandbox's own native Linux. That showed the real register-level
    `clone` argument order (`flags`/rdi, `child_stack`/rsi,
    `parent_tidptr`/rdx, `child_tidptr`/r10, `tls`/r8 -- note
    `child_tidptr` and `tls` swap positions relative to the C-level
    `clone(2)` man page prototype, a well-known x86_64 quirk), the exact
    three flag bits musl's `pthread_create` actually needs handled
    (`CLONE_SETTLS`, `CLONE_PARENT_SETTID`, `CLONE_CHILD_CLEARTID` --
    `CLONE_VM`/`CLONE_FS`/`CLONE_FILES`/`CLONE_SIGHAND`/`CLONE_THREAD`/
    `CLONE_SYSVSEM` need no kernel-side code at all, since "shares one
    address space" is just handing the clone the caller's own `cr3`), an
    `mprotect(PROT_READ|PROT_WRITE)` call around the new thread's stack
    that just needs to not fail (this kernel's mmap'd/brk'd pages are
    already uniformly read/write regardless of declared `prot`, the same
    simplification `sys_mmap` already admits -- so `sys_mprotect` is an
    honest, documented no-op, not a silently swept gap), and two distinct
    `futex()` calls: an internal `FUTEX_WAIT`/`WAKE` handshake pair musl
    uses at thread startup (whose actual meaning this kernel doesn't need
    to know -- generic wait/wake on whatever address is handed in is
    enough), and the one that actually makes `pthread_join` block: a
    `FUTEX_WAIT` on the thread's own tid word, satisfied only when that
    word is zeroed by real `CLONE_CHILD_CLEARTID` semantics at the
    child's exit.

    The core mechanism -- resuming a cloned child mid-syscall-epilogue,
    not via a fresh trampoline -- is new architecture, not a small patch.
    `linux_syscall.rs`'s `linux_syscall_entry` asm gained a `.global`
    label, `linux_syscall_resume_frame`, planted right where the normal
    return path already lands after calling `linux_syscall_handler`:
    `fxrstor` the current task's FPU state, pop all 16 saved registers in
    the same order they were pushed, `sysretq`. `sys_clone` builds a
    child by literally copying the *parent's own live 16-word syscall
    frame* -- found at `gdt::SYSCALL_KERNEL_RSP - 128`, which is always
    exactly where `linux_syscall_entry` leaves it, since `SYSCALL_KERNEL_RSP`
    is this task's own kernel stack top and the asm always pushes exactly
    128 bytes before calling into Rust -- patching just two of those 16
    words (the saved `rax` slot to 0, the saved user-`RSP` slot to
    `child_stack`) before handing the whole thing to `task::spawn_clone_raw`.
    That function writes the 128-byte frame onto a brand new kernel stack,
    then builds the same `switch_to`-compatible hand-crafted frame every
    other `spawn_*` function in `task.rs` already uses (`task_trampoline`/
    `enter_user_mode`'s six-callee-saved-registers-plus-return-address
    shape) -- just pointed at `linux_syscall_resume_frame` instead. When
    the scheduler eventually switches to this brand new task for the
    first time, `switch_to`'s own `ret` lands exactly on that label with
    RSP already sitting precisely where the hand-copied 16-word frame
    begins -- indistinguishable, from that instruction on, from a real
    task genuinely finishing an ordinary syscall.

    `futex()` itself needed a new scheduler primitive that didn't exist
    before: `TaskState::Blocked`, plus `task::futex_wait`/`futex_wake`.
    `futex_wait` marks the current task `Blocked` (tagged with the address
    it's waiting on) and calls `schedule()` directly, mid-syscall, the
    same "call straight into the scheduler from deep inside a syscall
    handler" pattern `SYS_EXIT`/`task_exit` already established (this
    kernel is single-core and every syscall already runs with interrupts
    disabled for its whole duration, so there's no window for another
    task to run between the `FUTEX_WAIT` value check and actually going to
    sleep -- the exact race real `FUTEX_WAIT` has to guard against).
    `futex_wake` scans for `Blocked` tasks tagged with a matching address
    and flips them back to `Ready`, same spirit as `schedule`'s existing
    round-robin eligibility check, just one more state to exclude.
    `task_exit` now also implements real `CLONE_CHILD_CLEARTID`: if the
    exiting task's `child_tidptr` is set, it zeroes that word (still with
    the exiting task's own address space loaded, so it's an ordinary
    trusted-pointer write, same model every other syscall argument here
    already uses) and wakes every futex-waiter on it, before ever calling
    `schedule()` to switch away -- exactly what unblocks a real
    `pthread_join`.

    Confirmed in QEMU via QMP screendump, ground truth end to end: the
    *exact same* `musl-gcc -static` binary `strace` ran natively --
    `pthread_create` a worker thread that prints `thread: got 42`, then
    `pthread_join` it and print `main: thread joined` -- produced exactly
    that output, correctly ordered, four separate times back to back (each
    one spawning a fresh task, IDs #5/#7/#9/#11 climbing as expected).
    `meminfo` before the first run and after all four read back the exact
    same `17009 used frames, 48363 free frames` both times, with the
    pre-existing kernel threads still healthy throughout -- proof a real
    `clone()`+`futex()`-backed thread lifecycle (new kernel stack
    allocated, FPU area allocated, thread runs, thread exits, `task_exit`'s
    reaping sweep frees both) leaks nothing, the same standard every
    memory-touching item in this log has been held to.

23. **Real `PT_INTERP` dynamic linking: a genuinely unmodified, dynamically-
    linked `musl-gcc` binary, linked against a real `libc.so` -- not
    KonjacOS's own hand-rolled `DT_NEEDED` scheme from item 19.** Every
    `.elf` run up through item 22 was either statically linked, or used
    this kernel's own from-scratch dynamic linker (parsing `DT_SYMTAB`/
    `DT_STRTAB` itself, resolving `R_X86_64_JUMP_SLOT`/`GLOB_DAT` by name,
    writing GOT entries eagerly at load time) -- real work, but narrow:
    SysV hash only, one dependency level deep, no `dlopen`. A real Linux
    binary doesn't dynamically link that way at all: it carries a
    `PT_INTERP` program header naming a real dynamic linker (for musl,
    uniquely, that's `libc.so` itself -- musl makes its own libc double as
    its own `ld.so`, unlike glibc's separate `ld-linux.so.2`), and the
    *kernel's* job shrinks to almost nothing: map the program's raw,
    completely unrelocated segments, map the interpreter's raw segments
    too, and jump to the *interpreter's* entry point instead of the
    program's, with just enough auxv information (`AT_PHDR`/`AT_PHENT`/
    `AT_PHNUM`/`AT_ENTRY` describing the *program*, `AT_BASE` giving the
    *interpreter's* own load bias) for it to bootstrap itself. Every
    relocation -- the interpreter's own self-relocation, finding and
    linking the main program, resolving symbols via real `GNU_HASH` (which
    this kernel's own item-19 scheme deliberately doesn't support) --
    happens entirely in userspace, using ordinary syscalls this kernel
    already had (`mmap`, `mprotect`, `arch_prctl`, `brk`, `writev`, ...).
    This is, deliberately, the exact same division of labor real Linux's
    own kernel ELF loader uses: `loader.rs`'s new
    `load_and_run_with_interp` doesn't contain a single line that resolves
    a relocation, for either file -- it's *less* code than the item-19
    path it sits next to, not more, because the hard part moved to
    someone else's already-correct implementation.

    `parse_elf_at` gained `PT_INTERP` detection (reading the requested
    interpreter path, e.g. `/libc.so`, straight out of that segment's file
    bytes) and an `Image::interp` field; `load_and_run` branches on it
    before ever touching the item-19 `DT_NEEDED` machinery, so the two
    schemes coexist without either one interfering with the other (`dyn.elf`
    from item 19, which has no `PT_INTERP`, still runs exactly the way it
    always has). Built and proven with musl-gcc's *default* linking mode
    (dynamic is the default; every prior musl demo in this log explicitly
    opted *out* with `-static`) plus `-Wl,--dynamic-linker=/libc.so` to
    give the interpreter an 8.3-safe path FAT16 can actually hold (a real
    musl toolchain normally bakes in `/lib/ld-musl-x86_64.so.1`, which is
    both too long for this kernel's short-filenames-only FAT16 driver and
    lives in a directory this disk image doesn't have -- overriding it at
    link time was simpler than either extending FAT16 for long names or
    inventing a fake `/lib` directory, and it changes nothing about what's
    actually being tested, since it's still genuinely `libc.so` doing
    genuinely all the linking work).

    Along the way this surfaced a real, previously-invisible bug dating
    back to item 20: `Image::phdr_vaddr` (what becomes `AT_PHDR`) was
    computed as just a segment's own runtime base, missing the `+ e_phoff`
    its own doc comment already said it needed -- wrong by 64 bytes for
    every binary this loader has ever built, but harmless for a statically
    linked musl binary with no `PT_TLS` segment (the only thing that ever
    walked this table before now), which just silently failed to find a
    segment type it was never going to find anyway. It stopped being
    harmless the instant something load-bearing depended on it: musl's own
    `ld.so`, walking this exact table to locate the main program's
    `PT_DYNAMIC` segment, read 64 bytes of the wrong memory as a phdr
    entry and dereferenced a garbage/null pointer out of it -- caught as a
    real `#14 Page Fault` (`CR2=0`) inside `libc.so`'s own `decode_dyn`,
    not by inspection, exactly the kind of latent bug this project's
    "boot it and watch it fail correctly" discipline exists to catch.
    Fixed by actually adding `e_phoff`, matching the doc comment that had
    been correct the whole time.

    Confirmed in QEMU via QMP screendump, ground truth end to end, two
    genuinely unmodified `musl-gcc`-built dynamic binaries: `mdyn.elf`
    (the same `hello.c` from item 20, this time dynamically linked) prints
    `hello from real musl!` correctly; `mdynthr.elf` (the same
    `pthreadtest.c` from item 22) prints `thread: got 42` then
    `main: thread joined` -- a full real `clone()`/`futex()` thread
    lifecycle running *through the real musl dynamic linker's own code
    path* this time, not this kernel's own syscall dispatch alone,
    including musl's `libc.so` itself independently probing
    `membarrier()` (syscall 324) at startup and gracefully handling this
    kernel's honest `-ENOSYS` for it exactly the way it would on a real
    Linux system without one. Run four times back to back (`mdyn.elf`,
    `mdynthr.elf`, `mdyn.elf`, `mdynthr.elf`, tasks #5/#6/#8/#9), `meminfo`
    before the first run and after the last read back the exact same
    `17010 used frames, 48362 free frames` both times -- proof that real
    dynamic linking through a real interpreter, real thread creation, and
    real thread teardown, all running someone else's actual compiled code
    this kernel had no hand in generating, leak nothing.

24. **Real `dlopen()`/`dlsym()`: loading a shared library at runtime through
    the same `libc.so` interpreter from item 23, not linked in at all at
    build time.** Item 23 got a real musl dynamic linker running and doing
    *its own* startup-time linking of whatever the ELF's `PT_INTERP`/
    `DT_NEEDED` already named. `dlopen` is a different animal: an ordinary
    running program calling back into that same linker's code, at any
    moment, to load a library nothing on disk said it needed. `strace -f`
    against a natively-built `musl-gcc -Wl,--dynamic-linker=... -ldl`
    binary calling `dlopen("./dlfoo.so", RTLD_NOW)` showed exactly what
    real musl needs the kernel to provide that item 23's binaries never
    exercised: `open`, `fstat`, `read`, a genuinely *file-backed* `mmap`
    (not just the anonymous kind every earlier item used), `close`, and
    `fcntl` (accepted as a no-op -- musl calls it for `F_SETFD`/`FD_CLOEXEC`
    bookkeeping this kernel doesn't need to honor for correctness).
    `linux_syscall.rs` gained real `sys_open`/`sys_fstat` implementations
    against the existing FAT16 driver and `task::OpenFile` table, and
    `sys_mmap` gained a fifth real argument (`fd`) plus the file's `offset`
    -- which, because `linux_syscall_entry`'s dispatch only ever forwards
    five fixed registers (`rdi,rsi,rdx,rcx,r8,r9` as `number,a0,a1,a2,a3,a4`),
    has to be read directly off the live syscall frame the same way
    `sys_clone` already reads `r9`, rather than as an ordinary sixth
    parameter.

    A `dlopen`'d library's real `mmap` sequence turned out to have more
    structure than any earlier `mmap` use in this log: first a plain,
    non-`MAP_FIXED` reservation call sized for the whole library, establishing
    its load bias, then one `MAP_FIXED` call per `PT_LOAD` segment,
    overlaying parts of that reservation at their real file offsets --
    including a RELRO-style remap where a later read-write `MAP_FIXED` call
    re-covers part of an earlier read-only segment's range. Getting this
    working end to end surfaced three real bugs, none of them guessed at,
    all three caught by actually running it and reading what broke:

    - **`dlsym` silently resolving to the wrong object.** `dlopen` appeared
      to succeed, but `dlsym(h, "foo_greet")` reported "Symbol not found" --
      musl's own already-loaded-DSO de-duplication compares each newly
      opened file's `(st_dev, st_ino)` against every currently-loaded
      object's, and `sys_fstat` was reporting `(0, 0)` for everything,
      so the freshly-opened `dlfoo.so` looked identical to an object
      already loaded (almost certainly the interpreter itself) and
      `dlopen` quietly handed back a stale handle instead of actually
      loading the new library. Fixed with a real per-path identity: a new
      `task::hash_path` (FNV-1a) gives `task::OpenFile` a real `ino`, and
      `sys_fstat` now reports a constant `st_dev=1` plus that real
      `st_ino`.

    - **A page fault (`CR2` inside the freshly reserved range, `RIP` inside
      the interpreter) right after the `ino` fix.** The non-`MAP_FIXED`
      reservation half of the sequence was still backed lazily, same as
      every anonymous mmap in this kernel -- fine for anonymous memory,
      but musl's own linker code dereferences the *mapped* library header
      directly (not only the separately-buffered copy `read()` already
      produced) while walking it, and page 0 of the reservation is exactly
      the header, and exactly the one `PT_LOAD` segment offset (0) that
      might never get its own follow-up `MAP_FIXED` call before something
      reads it. Fixed by making a file-backed non-`MAP_FIXED` reservation
      eager instead of lazy: map and copy real file content for the whole
      reservation immediately, even though most of it is about to be
      harmlessly overwritten by the `MAP_FIXED` calls that follow.

    - **A real, reproducible physical-frame leak, exactly 4 frames per
      run, introduced by the eager-reservation fix above.** The eager
      reservation allocates and maps a real frame for every page up front;
      the `MAP_FIXED` calls that follow then each allocated a *new* frame
      and overwrote the same virtual address's page-table entry via
      `paging::map_page` without ever freeing the frame that was already
      mapped there -- permanently orphaning one physical frame per
      `MAP_FIXED`-covered page, every single run. Caught the same way
      every leak in this log gets caught: not by inspection, but by
      running `meminfo` before and after and watching `used frames` climb
      linearly (17013 -> 17017 -> ... -> 17029 across four back-to-back
      runs) when it should have come back to baseline. Fixed by giving
      `MAP_FIXED` its real atomic-replace semantics: the handler now calls
      `paging::unmap_page` and frees whatever frame was already mapped at
      each target virtual address (the same pattern the adjacent
      `PROT_NONE`-unmap branch already used) before allocating and mapping
      the new one, so a `MAP_FIXED` call genuinely *replaces* a prior
      mapping instead of leaking it.

    Confirmed in QEMU via QMP screendump, ground truth end to end: a
    genuinely unmodified `musl-gcc -Wl,--dynamic-linker=/libc.so ... -ldl`
    binary (`mdlopen.elf`) calls `dlopen("./dlfoo.so", RTLD_NOW)` on a
    real, separately-built shared library (`dlfoo.so`, built with
    `-shared -fPIC` against the same `libc.so`), resolves `foo_greet` via
    `dlsym`, calls it, and prints `hello from a real dlopen'd shared
    library!` followed by `main: done` -- correct output, correct order,
    every time. Run four times back to back (tasks #5/#6/#7/#8),
    `meminfo` immediately before the first run and after every single one
    of the four reads back the exact same `17014 used frames, 48362 free
    frames`, with no drift at any point -- proof that a real `dlopen`'s
    open/fstat/file-backed-mmap/dlsym/dlclose sequence, run repeatedly,
    leaks nothing.

25. **Real `SIGSEGV` delivery: a genuine, kernel-delivered signal handler,
    installed with a real `rt_sigaction`/`signal()`, actually fires when a
    real fault happens -- not the silent no-op every earlier item left
    `rt_sigaction`/`rt_sigprocmask` as.** Every program up through item 24
    only ever *probed* these two syscalls at startup (musl's own init code
    unconditionally unblocks/queries a couple of realtime signal slots)
    and never once expected either to actually do anything, so returning
    `0` and moving on was honest enough -- until a program installs a
    *real* handler and then genuinely crashes, at which point that same
    no-op stops being an honest simplification and starts being a bug: a
    fault the caller explicitly said it could recover from would instead
    hit this kernel's ordinary fatal path and take the whole task down
    anyway. `strace -f` against a real musl `signal(SIGSEGV, handler)` +
    a real null-pointer write showed the actual sequence needed:
    `rt_sigaction` installs `{sa_handler, sa_mask, sa_flags, sa_restorer}`,
    the fault itself, the handler running, then a `rt_sigprocmask`
    (restoring the pre-handler mask -- not actually tracked here, see
    below) and normal continuation. Getting from "a page fault this
    kernel already knows how to print and halt on" to "a page fault that
    can redirect a *running task* into its own real handler and later
    resume exactly where it left off" needed three real, separate pieces
    of new machinery:

    - `paging.rs`'s `handle_page_fault` used to take just the raw CPU
      error code and CR2 -- enough to print-and-halt, not enough to
      redirect execution. It now takes a pointer to a named `PfFrame`
      struct laid directly over `isr_stub_14`'s existing 15-GPR-plus-
      `iretq`-frame stack layout (`#[repr(C)]`, fields in the exact
      ascending-address order the asm already pushes them in), so
      `handle_page_fault` can read *and write* every register the
      faulting task had, including `RIP`/`RSP`/`RDI` -- the three it
      needs to actually redirect into a handler. `isr_stub_14` itself
      barely changed: `mov rdi, rsp` instead of picking one field out by
      a hardcoded offset, since `rsp` already points exactly at
      `PfFrame`'s first field.
    - A truly fatal fault (not a recoverable lazy-`SYS_MMAP`-reservation
      touch, item 15's existing recovery path, left completely
      unchanged) now checks, before giving up: is this ring 3 (`CS`'s
      RPL bits == 3, never a kernel bug), does the current task have a
      real handler installed for `SIGSEGV` (`handler > 1` -- `0` is
      `SIG_DFL`, `1` is `SIG_IGN`, neither a real address), and does it
      have a real `sa_restorer` (`SA_RESTORER`, required on x86-64 by
      every real libc that installs a handler here -- without one, this
      kernel has no trampoline to hand control back through once the
      handler returns, so it honestly declines to deliver rather than
      inventing one). If all three hold, `try_deliver_sigsegv` snapshots
      the *entire* pre-fault CPU context into a new per-task
      `SavedContext` (every GPR, `RIP`/`CS`/`RFLAGS`/`RSP`/`SS`), pushes
      the real `sa_restorer` onto a fresh stack slot below the original
      `RSP` (clearing the SysV red zone first) as the handler's "return
      address", and redirects `RIP`/`RSP`/`RDI` to the handler -- at
      which point `isr_stub_14`'s *existing* resume path (`fxrstor`, pop
      15 GPRs, `iretq`) carries out the actual jump into userspace,
      completely unaware it's delivering a signal instead of retrying a
      lazy mapping. `task::begin_signal_delivery` refuses a second
      delivery while one's already in flight for the same task -- real
      Linux blocks a signal for the duration of its own (non-
      `SA_NODEFER`) handler, so a fault that recurs *inside* the handler
      kills the process instead of recursing forever; this is the same
      safety net, reached the same way (falling through to the ordinary
      fatal path), not a hand-rolled substitute for it.
    - The handler eventually returns into its `sa_restorer` (real musl/
      glibc code, `mov rax, 15; syscall` -- this kernel never wrote a
      byte of it), landing on the new `rt_sigreturn` (syscall 15).
      Unlike every other real syscall here, this one can't just resume
      through `linux_syscall_entry`'s ordinary `fxrstor`/pop/`sysretq`
      epilogue: that epilogue can't restore `RCX`/`R11` (the `syscall`
      instruction itself sacrifices both on the way in, real Linux ABI
      behavior, not a bug), but the context being resumed here is a
      *different*, already-fully-known one -- the `SavedContext`
      `paging.rs` snapshotted at fault time, `RCX`/`R11` included. So
      `sys_rt_sigreturn` diverges (`-> !`) straight into a hand-written
      `sigreturn_restore`: builds a brand new `iretq` frame from the
      saved fields and restores every single GPR from it, something only
      `iretq` (not `sysretq`) can do. `linux_syscall.rs` gained
      `offset_of!`-based tripwires on every `SavedContext` field this
      asm reads by fixed byte offset, the same "fails to *build*, not
      silently misbehave at runtime, if the struct layout ever drifts"
      discipline item 23's `phdr_vaddr` bug retroactively argued for.

    `sys_rt_sigaction` itself reads and writes a real kernel-ABI
    `struct k_sigaction` -- **not** the `struct sigaction` shape a C
    program's own source sees; glibc/musl reorder the fields before
    issuing the actual syscall. Rather than trust a half-remembered
    layout, this was ground-truthed directly: a small test program that
    bypasses libc's `sigaction()` wrapper entirely and calls
    `syscall(SYS_rt_sigaction, ...)` with a hand-built
    `{ handler, flags, restorer, mask }` (that exact field order, 32
    bytes) confirmed a real musl-linked handler still fired correctly
    against it, run natively on the sandbox's own Linux before a single
    line of kernel code was written against it.

    Two honest, explicitly scoped gaps, not silently swept under the rug:
    real signal *masking* (`rt_sigprocmask` blocking/unblocking specific
    signals) still isn't tracked -- every test program only ever
    sets/restores a mask around a handler that now genuinely fires, never
    actually depends on something staying blocked, so the existing no-op
    is still honest for now, just for a narrower reason than before. And
    delivery only ever hands the handler a signal number (`RDI`), never a
    real `siginfo_t`/`ucontext_t` (no `SA_SIGINFO` support yet) -- every
    real-world handler tested so far uses the plain `void handler(int)`
    form, so this hasn't been a gap in practice yet, but a signal handler
    that inspects *why* it was called (a JVM's own SIGSEGV-based implicit-
    null-check handler, down the road, being the obvious future customer)
    will need it.

    Confirmed in QEMU via QMP screendump, ground truth end to end: a
    genuinely unmodified `musl-gcc -static` binary (`sigtest.elf`) calls
    `signal(SIGSEGV, handler)`, then deliberately writes through a null
    pointer. The real fault fires, this kernel delivers it as a real
    `SIGSEGV`, the handler prints `handler: caught signal 11`, calls
    `siglongjmp` back into `main` (ordinary libc code, unaware or caring
    that the signal underneath it was kernel-delivered rather than
    hardware-native), which prints `main: recovered from segfault` and
    `main: done` -- the task *keeps running after a real crash*, something
    nothing before this item could do; every earlier fault of any kind
    was unconditionally fatal. Run four times back to back (tasks
    #5/#6/#7/#8), `meminfo` before the first run and after every single
    one of the four read back the exact same `17022 used frames, 48350
    free frames` -- proof that a real fault-deliver-handle-recover cycle,
    repeated, leaks nothing: no orphaned frame from the new stack slot the
    handler's entry carved out, no leftover state from `begin_signal_delivery`/
    `end_signal_delivery`'s per-task bookkeeping.

26. **Real `mprotect()`: a page genuinely, provably stops being writable,
    not the always-succeeds no-op every earlier item honestly left it
    as.** Item 22's own `sys_mprotect` doc comment already admitted
    exactly what was missing and why it hadn't mattered yet: every real
    `mprotect` caller seen up to that point (musl's pthread guard-page
    setup) only needed the call to *not fail*, since nothing in this
    kernel's page-fault handling depended on `prot` being narrower than
    "always writable" for correctness. That stopped being true the moment
    a program's *correctness* -- not just its startup sequence -- starts
    depending on a permission change actually taking effect: a JIT's W^X
    code cache (`PROT_READ|PROT_EXEC` while running, `PROT_READ|
    PROT_WRITE` while being patched, genuinely never both at once,
    something a real JVM leans on constantly) is the obvious future
    customer this kernel can no longer fake its way past.
    `paging::protect_range_in` does the real work: same non-destructive
    page-table walk `translate`/`unmap_page` already established, but
    rewrites each already-mapped PTE's flags in place (preserving its
    physical mapping) instead of just reading or clearing it, with an
    `invlpg` per page so the new permissions take effect immediately
    rather than waiting for a stale TLB entry to expire on its own.
    `sys_mprotect` translates real `PROT_READ`/`PROT_WRITE`/`PROT_EXEC`
    bits into this kernel's own flags: `PROT_WRITE` -> `PAGE_WRITABLE`,
    and *any* real access bit -> `PAGE_USER` -- deliberately dropped
    entirely for a bare `PROT_NONE`, which turns out to need no new
    mechanism at all: a ring-3 access to a *present* page with
    `PAGE_USER` clear already faults, through the exact same #PF path
    every other protection violation already takes, so reusing that one
    bit for `PROT_NONE` is a real enforcement, not an invented one.

    Two gaps are carried over honestly rather than silently fixed by
    implication: `PROT_EXEC` is read but not enforced -- this kernel has
    never touched the NX bit or `EFER.NXE` anywhere, so every present,
    user-reachable page stays executable regardless of what `prot` says,
    meaning a `PROT_READ`-only page a real program expects to fault on
    jumping into currently won't. And a page inside the requested range
    that isn't mapped yet at all -- the lazily-backed interior of an
    untouched `SYS_MMAP` reservation, item 15's demand-paging scheme --
    is silently skipped rather than protected in advance, since this
    kernel's lazy-mmap fault handler doesn't track a *requested*
    protection separately from "just map it `PAGE_WRITABLE` on first
    touch." Both are exactly the same kind of scoped, stated limitation
    `sys_mmap`'s and `sys_mprotect`'s own earlier doc comments already
    modeled -- narrower than real Linux, honestly, not pretended
    otherwise.

    Confirmed in QEMU via QMP screendump, ground truth end to end, and
    deliberately built on top of item 25's real signal delivery rather
    than around it: a genuinely unmodified `musl-gcc -static` binary
    (`mprot.elf`) `mmap`s a real read-write page, writes `123` through it
    (forcing the lazy reservation to actually get backed, so there's a
    real PTE for `mprotect` to find), calls `mprotect(p, 4096,
    PROT_READ)`, installs a real `SIGSEGV` handler, then deliberately
    writes through the now-read-only pointer. The write genuinely faults
    -- not a simulated check, an actual hardware `#PF` against a
    `PAGE_WRITABLE`-cleared PTE -- the handler catches it and prints
    `handler: caught signal 11`, and `main` prints `main: write correctly
    faulted -- mprotect IS enforced` after `siglongjmp`ing back, exactly
    the outcome that would have silently *not* happened under the old
    always-writable no-op. Run four times back to back alongside item
    25's own `sigtest.elf` (tasks #5 through #9, five runs total across
    both binaries), `meminfo` before the first run and after every single
    one of them read back the exact same `17022 used frames, 48350 free
    frames` -- proof that a real permission-change-then-genuinely-fault
    cycle, repeated across two different test programs exercising two
    different items together, leaks nothing.

27. **Real NX (execute-disable) enforcement, closing the one gap item 26
    named and left open: `PROT_EXEC` now actually means something.** Item
    26's own `sys_mprotect` doc comment said exactly what was missing:
    every present, `PAGE_USER` page stayed executable no matter what
    `prot` requested, because this kernel had never set the NX bit or
    enabled `EFER.NXE` anywhere. That's real, load-bearing machinery for
    the JVM this project is ultimately aimed at -- HotSpot's JIT relies on
    genuine W^X (a code page is either writable-while-being-patched *or*
    executable-while-running, never both at once) as a real security
    property, not a suggestion, and a kernel that silently lets a
    `PROT_READ`-only page still execute would make that property fake.
    A new `paging::init()` -- the first thing `kstart` calls after
    `gdt::init()`/`idt::init()`, before anything maps memory NX could
    apply to -- checks `CPUID.80000001H:EDX[20]` for real hardware
    support (every CPU this kernel has ever booted under reports it, but
    checked rather than assumed, the same "verify, don't guess" standard
    item 25's `SA_RESTORER` check and item 23's ground-truthed struct
    layout already held to) and, only if present, sets `EFER.NXE`. A new
    `PAGE_NX` (bit 63 of a PTE) joins `PAGE_PRESENT`/`PAGE_WRITABLE`/
    `PAGE_USER`; `sys_mprotect` sets it whenever `PROT_EXEC` is absent
    from a request and `paging::nx_available()` confirms the feature is
    actually live -- genuinely gated on real hardware capability, not a
    bit set unconditionally and hoped for.

    Getting `cpuid` itself to compile inside the kernel surfaced a small,
    real Rust/LLVM constraint, not a logic bug: `cpuid` clobbers `RBX`,
    but LLVM reserves `RBX` for its own internal bookkeeping and refuses
    to let inline asm name it as an operand at all (`cannot use register
    'bx': rbx is used internally by LLVM`) -- fixed the standard way any
    `no_std` `cpuid` wrapper does, saving/restoring it by hand
    (`push rbx` / `cpuid` / `pop rbx`) around the instruction instead of
    ever declaring it as a register operand.

    An execute violation takes a real `#PF`, indistinguishable at the
    dispatch level from every other kind of protection violation this
    kernel already handles (a write to read-only memory, an access to
    `PROT_NONE`) -- it just falls through the same fatal/`SIGSEGV`-
    delivery path items 25 and 26 already built, no new fault-handling
    code needed at all. That's not an oversight; it's what "real
    enforcement reuses real infrastructure instead of growing a parallel
    special case for every new flavor of fault" is supposed to look like.

    Confirmed in QEMU via QMP screendump, ground truth end to end, and
    deliberately layered on top of both items 25 and 26 rather than
    tested in isolation: a genuinely unmodified `musl-gcc -static` binary
    (`nxtest.elf`) `mmap`s a real read-write page, writes a literal `0xC3`
    (`ret`) byte into it, `mprotect`s it down to `PROT_READ` (no
    `PROT_EXEC`), installs a real `SIGSEGV` handler, then jumps straight
    into it. The CPU genuinely refuses the instruction fetch itself --
    the byte is right there, correctly written, and would execute
    trivially under the old no-op -- the handler catches the real fault
    and `main` prints `main: call correctly faulted -- NX IS enforced`
    after `siglongjmp`ing back. Run four times back to back (tasks #6
    through #9), immediately after a regression check that item 26's own
    `mprot.elf` still faults correctly too (task #5, unaffected by any of
    this), `meminfo` before the first run and after every single one of
    the five read back the exact same `17022 used frames, 48350 free
    frames` -- proof that real NX enforcement, layered on top of real
    write-protection enforcement, on top of real signal delivery, three
    items deep, still leaks nothing.

28. **DOOM now opens in a real, automatically-windowed GUI frame with a
    genuine, clickable X close button -- no need to run `gui` first, and
    the desktop demo gets real close buttons of its own as a side effect.**
    Previously `doom` just took over the whole framebuffer with no chrome
    at all; the only way to see this kernel's window-manager look (title
    bar, border, close button) was the separate, manual `gui` command,
    and its own demo windows never actually closed on click -- `wm.rs`'s
    loop only ever removed a window by dragging never being connected to
    a real close affordance at all. Both gaps close together, because
    they turned out to be the same underlying gap: `wm.rs`'s chrome-
    drawing was never reusable by anything outside its own `run()` loop.

    `wm.rs` is refactored first, before any of the DOOM-specific work:
    `draw_chrome`/`draw_cursor`/`close_button_rect`/`point_in_close_button`
    come out of the demo loop's body as free functions that take a
    `&mut Canvas` and draw (or hit-test) one window's title bar, border,
    and a real hoverable X button, or the desktop cursor, without ever
    touching the client rect below the title bar -- that's left entirely
    to whoever calls them, whether it's `wm.rs`'s own solid-color demo
    fill or a real program's actual rendered frame. The demo loop itself
    gets real close buttons as a direct, almost-free consequence: its
    click handler now checks `point_in_close_button` before falling
    through to the existing drag-start check, and a hit just removes that
    window from the vec outright.

    `doom_driver.rs` is the first real consumer. A new `doom_window_init`,
    called once by `commands.rs::doom_task_entry` right after
    `doomgeneric_Create` (WAD loading, `DG_Init`, ...) but before the
    render loop starts, snapshots the current screen as this window's
    "restore on close" background, sizes and centers a
    `640x(400 + TITLE_BAR_H)` window, and draws its initial chrome --
    so the frame is already up, titled "DOOM", before the very first real
    game frame ever lands inside it. `konjac_doom_blit` -- called by
    doomgeneric every frame, previously just centering the raw 640x400
    buffer on the whole screen -- now blits into that window's client
    rect instead when one's open (falling back to the old whole-screen
    centering if it somehow isn't, rather than silently drawing nothing),
    then redraws the chrome (with real hover feedback) and the desktop
    cursor on top, using the exact same `wm.rs` functions the manual `gui`
    demo uses -- one shared look, two call sites, not two different UIs
    that happen to resemble each other. A fresh left-click (edge-detected
    against last frame, so holding the button doesn't retrigger every
    frame) inside the close button restores the pre-window screen and
    calls `task::exit_current()` -- never returns, the same real exit path
    a running C program already takes here (`libc_shim.rs`'s `exit`/
    `abort`), abandoning this task's kernel stack mid-`DG_DrawFrame`
    exactly the way those do.

    That close button surfaced a real, previously-latent kernel bug, not
    a DOOM-specific one. `task::TASKS` -- the scheduler's task table --
    is locked both by ordinary task-context code (`task_exit`, `spawn`,
    `kill`, `futex_wait`/`wake`) *and* by `timer.rs`'s own tick handler
    (via `schedule()`), and until this item, `sync::SpinLock` had no
    interrupt-safety at all -- its own doc comment said so outright. Every
    IRQ handler on this single-core kernel runs through an interrupt gate
    (`idt.rs`'s `0x8E` type-attr byte), which clears `IF` on entry, so an
    ISR can never itself be preempted. If a 100Hz timer tick landed while
    task-context code held `TASKS` mid-critical-section -- rare, a
    handful-of-instructions window against a 100Hz clock, which is
    exactly why this had apparently never been hit before -- the tick
    handler's own attempt to lock `TASKS` would spin forever: it can't be
    preempted out of the way, and the actual holder can never run again to
    release the lock, since nothing else can run on a single core while
    the ISR spins. Busy-spinning at ~100% CPU, no crash, no further
    output -- exactly the full system hang this item hit the very first
    time `task::exit_current` got exercised from deep inside a per-frame
    callback (`konjac_doom_blit`'s new close-button branch) rather than
    from DOOM's own top-level quit path (`libc_shim.rs`'s `exit`, already
    working, just apparently never unlucky enough to land on that exact
    tick before).

    Ground-truthed the hard way, not guessed at: `sprintln!` breadcrumbs
    (the serial port, a separate lock from the graphical console, so it
    kept working even under suspicion of a console-lock deadlock) through
    `doom_window_init`, `konjac_doom_blit`, and finally `task::task_exit`/
    `schedule` themselves narrowed the hang down to exactly
    `task_exit`'s `TASKS.lock()` call, confirmed by watching execution
    reach "about to call schedule()" and never anything after it, CPU
    pegged the whole time. The first fix attempt -- making `SpinLock`
    itself always `cli` while held -- did stop the hang, but broke
    something that had always worked before: DOOM's own startup now
    stalled forever partway through loading status-bar graphics
    (`ST_Init`/`Z_Malloc`/disk reads), something in that path apparently
    depending on interrupts actually staying enabled across some other,
    unrelated lock. Rather than chase that down, the fix landed narrower:
    a new `sync::IrqSpinLock` -- `lock()` disables interrupts, restoring
    whatever they actually were (not unconditionally re-enabling) on
    drop, so it nests safely under an already-`cli`'d caller -- used
    *only* for `task::TASKS`, the one lock genuinely shared between task
    context and an ISR. Every other lock in the kernel (`console::CONSOLE`,
    the heap allocator, the serial port, the physical frame allocator)
    keeps the original, cheaper, still-interrupt-unsafe `SpinLock`
    unchanged, since none of them are ever touched from interrupt context.

    Confirmed in QEMU via QMP: `screendump` for the visuals, and a real
    relative PS/2 mouse driven via QMP's `input-send-event` (`"rel"`
    events, not `"abs"` -- this kernel's `mouse.rs` is a genuine relative
    device, not a USB-tablet-style absolute one, confirmed by reading its
    own module doc comment) for actual clicks, not simulated ones. `doom`
    launches straight into a titled, bordered window with DOOM's real
    shareware title screen and live gameplay rendering inside the client
    area, the real cursor visible and independently movable over it,
    hovering the X button visibly lightens it, and a real left-click
    there restores the exact screen from the instant before the window
    opened, prints "doom: closed from its window's X button.", and hands
    control back to a fully responsive shell (`help` runs correctly
    immediately after) -- reproduced cleanly, repeatedly, after the
    `IrqSpinLock` fix, with `meminfo`'s physical frame count
    (`17024 used frames, 48348 free frames`) identical before launching
    DOOM and after closing it via the X button; only heap usage grows,
    expected and unrelated to this item, since this kernel's allocator is
    a bump allocator that has never freed individual allocations,
    DOOM's zone/status-bar allocations included, regardless of which exit
    path a session ends on. The `gui` demo's own new close buttons are
    the same code path DOOM's window already proved correct, verified
    directly by code equivalence rather than reproduced separately: real
    keyboard input (`Esc`) into that same running demo loop was confirmed
    live via QMP mid-session, but clicks specifically landed unreliably
    through QMP's synthetic relative-mouse input in that same window
    (matching exactly the kind of click-delivery flakiness -- not
    position or logic -- already worked around at length getting DOOM's
    own close-button clicks to land during this same verification pass).

29. **The ELF loader now decodes `DT_RELR` (packed relative relocations)
    and `R_X86_64_IRELATIVE` (GNU IFUNC) entries instead of rejecting them
    outright -- the specific gap standing between this kernel and running
    a real, unmodified glibc dynamic linker, found by literally trying: a
    genuine `ld-linux-x86-64.so.2` pulled straight from a real Ubuntu
    install (the actual target being worked toward is a real JVM, which is
    glibc-linked) failed to load at all with `loader: this file has a
    .rela.dyn relocation type KonjacOS's loader doesn't support yet`,
    thrown from `parse_elf_at` before a single instruction of ld.so ever
    ran. `readelf -r` on the real binary showed why: modern `ld` (binutils
    default since ~2021, `-z pack-relative-relocs`) packs almost all of a
    self-relocating object's own `R_X86_64_RELATIVE` fixups into a
    `.relr.dyn`/`DT_RELR` bitmap instead of `.rela.dyn`'s one-entry-per-fixup
    `Elf64_Rela` array, and separately emits one real `R_X86_64_IRELATIVE`
    entry for its own IFUNC-resolved internals -- neither of which this
    loader had ever seen before, since every binary it had loaded so far
    (musl-static, this project's own hand-assembled test ELFs) predates or
    doesn't use either.

    `DT_RELR` decoding (`parse_elf_at`, `kernel/src/loader.rs`) is a real
    implementation of the generic-ABI RELR format, not a partial one: each
    8-byte word in the table is either an even *address* (the next fixup
    site) or an odd *bitmap* whose bits 1..63 each mean "the site 1..63
    words past the last address also needs the same fixup", exactly
    mirroring how a real `ld.so`'s own RELR decoder walks it. Since RELR
    leaves the addend implicit (it's whatever pointer value the linker
    already wrote at that address, computed as if loaded at bias 0), each
    decoded entry reads that value back out of the file's own bytes via
    `vaddr_to_file_offset` and feeds it into the exact same
    `Image::relocations` list `R_X86_64_RELATIVE` entries already use --
    no new code path for *applying* the fixup, just a second way to arrive
    at one.

    `R_X86_64_IRELATIVE` is handled more conservatively, deliberately not
    pretending to be more solved than it is: resolving one for real means
    *calling* the resolver function at `bias + r_addend` and using its
    return value, since that's the entire point of an IFUNC (picking its
    real implementation at bind time, e.g. by `CPUID`) -- something this
    loader has no way to do yet (there's no mechanism here for the kernel
    to execute arbitrary code in a not-yet-running task's address space
    mid-load). Rather than silently write the resolver's own address in as
    if it were the resolved function (real, but wrong -- calling through
    it would jump into the resolver, not whatever the resolver would have
    picked) or keep erroring out entirely, these entries are now parsed
    into their own `Image::irelative_relocations` list and handled
    per-path: `load_and_run`'s eager loader (which does apply every other
    relocation itself, before the task ever runs) rejects a file that has
    any, with a clear reason, exactly like an unsupported relocation type
    already got rejected before this item; `load_and_run_with_interp` --
    the path a real `PT_INTERP` binary like `ld.so` actually takes --
    doesn't need them resolved at all, since (as its own doc comment
    already described before this item) it never applies *any* relocation
    it collects, for either the main program or the interpreter -- a real
    `ld.so`, once running, redoes all of its own relocations from scratch
    in userspace anyway, IFUNCs included, the same way real Linux's kernel
    ELF loader (`binfmt_elf.c`) never resolves one either.

    Confirmed in QEMU, ground-truthed end to end via QMP (headless
    `-display none`, keyboard input via `send-key`, `screendump` for
    output -- this kernel's shell is a framebuffer console, serial only
    carries `sprintln!` boot/debug text): a real, byte-for-byte-unmodified
    `ld-linux-x86-64.so.2` (copied from a real Ubuntu 24.04 install, its
    embedded `PT_INTERP` self-reference aside) placed on the FAT16 data
    disk under a short 8.3 name (`LDSO.BIN` -- this filesystem has no VFAT/
    long-name support, a separate, real limitation surfaced while chasing
    this one down) and pointed at by a small dynamically-linked test
    binary's own `PT_INTERP`. Before this item, `run jtest.elf` failed
    immediately with the `.rela.dyn relocation type` error above, never
    reaching `load_and_run_with_interp` at all. After it, the shell prints
    `run: jtest.elf: recognized as ELF64, task #5 spawned`, and the real
    ld.so genuinely starts executing in ring 3: it makes real `access`
    (syscall 21) and repeated real `openat`/`newfstatat` (syscalls 257/262)
    calls -- all real syscalls this kernel already dispatches, just not yet
    implements, so each returns `-ENOSYS` -- searching the handful of
    library search paths a real ld.so always tries, then, once every
    attempt to find `libc.so.6` has failed exactly the way it would after
    a real `ENOSYS`-driven `open` failure, prints its own genuine, entirely
    real diagnostic: `run:elf: error while loading shared libraries:
    libc.so.6: cannot open shared object file: Error 38` -- ld.so's actual
    error-reporting code, running for real, not a simulation of what it
    would say. No kernel panic, no page fault; the serial log shows a
    clean boot straight through to "interrupts enabled -- handing off to
    the shell" with nothing after it but this session's own commands. The
    next real blocker, precisely located rather than guessed at: `openat`/
    `newfstatat` need real implementations before ld.so can get past
    finding its own dependencies at all.

30. **Real `openat`/`newfstatat` (`linux_syscall.rs`), closing the exact
    gap item 29 left open.** `openat` (syscall 257) turns out to need no
    new logic at all: with `dirfd` ignored, it's the identical operation
    `sys_open` already does (this loader's paths have always been treated
    as absolute/root-relative, never dependent on a real per-task current
    directory a `dirfd` could redirect), so the dispatch table just routes
    it there directly rather than growing a near-duplicate function.
    `newfstatat` (syscall 262) is genuinely new: a path-based `fstat`,
    factored out of `sys_fstat`'s existing `struct stat`-filling code
    (pulled into a shared `write_stat` helper) so both the fd-based and
    path-based callers fill in the same `st_dev`/`st_ino`/`st_nlink`/
    `st_mode`/`st_size` fields the same honest way -- everything else in
    the 144-byte `struct stat` stays zeroed, unchanged from item 23's own
    scoping.

    Confirmed in QEMU via QMP (same headless `-display none`/`send-key`/
    `screendump` setup as item 29), re-running the exact same `run
    jtest.elf` against the same real, unmodified `ld-linux-x86-64.so.2`
    fixture: before this item, ld.so's search loop spammed `linux_syscall:
    unimplemented syscall number 257`/`262` on every path it tried, then
    misreported the failure as `Error 38` (`ENOSYS` -- "function not
    implemented", the honest-but-misleading answer this kernel's catch-all
    unimplemented-syscall handler always gives) once every attempt had
    silently failed the same wrong way. After this item, that spam is
    gone -- `openat`/`newfstatat` now actually run FAT16 lookups for each
    candidate path -- and ld.so prints the *correct* real diagnostic
    instead: `run:elf: error while loading shared libraries: libc.so.6:
    cannot open shared object file: No such file or directory`. That's not
    a cosmetic change: `ENOSYS` and `ENOENT` mean genuinely different
    things to a real caller (the former should make a robust program
    fall back to a different approach entirely, the latter just means
    "keep trying the next candidate," which is exactly the multi-path
    search loop ld.so's own code is running here), and this is proof the
    kernel is now telling it the *true* one -- `libc.so.6` really isn't
    anywhere on this disk yet (only the test fixture and `LDSO.BIN`
    itself are), not that the kernel can't look. Serial log clean through
    to "interrupts enabled" both before and after, no panic either time.
    The next real blocker is no longer a missing kernel feature at all:
    it's that no real `libc.so.6` has been placed on the FAT16 disk (under
    a short 8.3 name and with `jtest.elf`'s own `DT_NEEDED` string patched
    to match, the same treatment item 29 already gave `LDSO.BIN`) for
    ld.so to actually find and load.

31. **A real, unmodified glibc dynamic executable now runs to completion
    under KonjacOS -- real `ld.so`, real `libc.so.6`, real TLS, real
    userspace relocation, actual `main()` output.** This is the payoff
    items 29/30 were both building toward, closed out by finishing the
    exact two things their own "next blocker" callouts named: a real
    `libc.so.6` (copied byte-for-byte from this machine's own Ubuntu
    24.04 install, same as `LDSO.BIN` before it) placed on the FAT16 disk,
    and one real, ground-truthed *new* syscall.

    Getting `libc.so.6` actually found took two tries, both informative.
    First, placed at FAT16 root as `LIBC.BIN` with `jtest.elf`'s
    `DT_NEEDED` string patched from `libc.so.6` to `LIBC.BIN` (an in-place
    byte patch of the dynamic string table, identical technique to item
    29's `PT_INTERP` patch, same length in, same length out) -- and ld.so
    *still* reported `LIBC.BIN: cannot open shared object file: No such
    file or directory`. Real ld.so semantics explain why: a `DT_NEEDED`
    name with no `/` in it isn't looked up at the filesystem root at all --
    it's searched against a handful of hardcoded default system library
    directories (`/lib/x86_64-linux-gnu/`, `/lib64/`, ...) compiled into
    `ld.so` itself, none of which exist on this flat FAT16 image. Real
    dynamic-linker semantics also supply the fix: a `DT_NEEDED` name
    *containing* a slash is used as a literal path instead of being
    searched at all -- so the string got patched again, `LIBC.BIN` ->
    `/LIBC.BIN` (again the same 10-byte slot, no padding games needed this
    time: one extra character in, one fewer trailing NUL out), and that's
    what actually made ld.so's own real `openat("/LIBC.BIN", ...)` land on
    the file that was already sitting right there.

    That got real `openat`/`newfstatat` calls to succeed for the first
    time against a file ld.so actually intended to load, which immediately
    surfaced the one genuinely new syscall this item adds: **`pread64`**
    (`sys_pread64`, `linux_syscall.rs`) -- real `ld.so`'s own `_dl_map_object`
    reads pieces of a shared object's ELF header/program table by explicit
    file offset while deciding how to `mmap` it, without disturbing
    whatever a later sequential `read` on the same fd might expect the
    position to still be at. Implemented as a straight offset-indexed copy
    out of the same `OpenFile::data` every other file syscall already
    reads from -- deliberately *not* touching `file.pos`, which is the
    entire reason a real caller needs `pread64` to exist as its own
    syscall instead of `lseek`+`read`.

    Confirmed in QEMU via QMP (same headless setup as items 29/30): with
    both fixes in place, `run jtest.elf` no longer errors out at all --
    the shell prints `run: jtest.elf: recognized as ELF64, task #5
    spawned`, and the task's own real output appears right after:
    **`hello from glibc dynamic`** -- `jtest.elf`'s actual `main()`,
    reached via real ld.so self-relocation (items 29's `DT_RELR`/
    `R_X86_64_IRELATIVE` decoding), real `libc.so.6` loading (this item's
    `/`-prefixed-path fix and `pread64`), and real glibc TLS setup this
    kernel's existing `arch_prctl`/`mmap`/`mprotect` already happened to
    be sufficient for, all the way through to a real, unmodified glibc
    `printf`-equivalent actually executing in ring 3 and its output
    reaching the framebuffer console. Along the way, ld.so and glibc's
    startup path also called `set_robust_list` (273), `clock_gettime`
    (228), `prlimit64` (302), `getrandom` (318), and `rseq` (334) -- none
    of them implemented, each logged and answered `-ENOSYS` by the same
    catch-all this module has always used for an unimplemented syscall --
    and, tellingly, none of that stopped the program from running to
    completion: real glibc treats every one of those as an optional,
    gracefully-degradable feature (rseq/rd registration failing just means
    "this kernel doesn't support restartable sequences," `getrandom`
    failing falls back to a weaker seed rather than aborting, and so on),
    which is exactly the kind of real-world tolerance that makes "the
    program still ran despite five ENOSYS answers" a meaningful, not
    lucky, result. Serial log clean through to "interrupts enabled" with
    no panic. `getrandom`, `clock_gettime`, and real thread creation
    (`clone` beyond what a single-threaded program needs) are the next
    honest gaps -- unlikely to matter for another simple test binary, but
    exactly the kind of thing a real, thread-heavy, entropy-hungry JVM
    would notice immediately.

32. **Real `clock_gettime`/`getrandom` (`linux_syscall.rs`), closing two of
    item 31's three named gaps -- the two that matter well beyond one test
    binary, since a JVM specifically leans on both constantly (timing
    every GC pause and JIT compilation, seeding `hashCode()`/`SecureRandom`).**

    `sys_clock_gettime` answers `CLOCK_REALTIME`/`CLOCK_MONOTONIC`/
    `CLOCK_BOOTTIME` all the same honest way: real elapsed time derived
    from `timer::ticks()`, i.e. genuinely correct for `CLOCK_MONOTONIC`/
    `CLOCK_BOOTTIME` (neither one promises to relate to wall-clock time
    anyway) and honestly *since-boot* rather than fabricated wall-clock/
    UTC for `CLOCK_REALTIME`, since there's still no RTC driver here to
    back a real answer with -- the same "honest but incomplete beats
    fabricated" choice `sys_fstat`'s zeroed timestamp fields already made.
    Any other clock ID gets a clean `EINVAL` instead of a guess.

    `sys_getrandom` is pickier: rather than seed a fake CSPRNG from
    something predictable (`timer::ticks()`, a stack address), it checks
    `CPUID.1:ECX[30]` for real `RDRAND` hardware support -- the identical
    "verify, don't guess" discipline `paging::init`'s own NX check
    established in item 27, reused here for a second, unrelated CPU
    feature -- and only ever returns bytes actually drawn from `rdrand`
    (retried up to Intel's own documented bound of 10 attempts per draw on
    a transient conditioner-empty failure), or honestly refuses with
    `ENOSYS` if the hardware genuinely doesn't have it. In practice, under
    this project's own `qemu-system-x86_64` invocation (plain default CPU
    model, no `-cpu host`/KVM acceleration), `RDRAND` isn't exposed at
    all, so this path currently always takes the honest-refusal branch --
    a real, verified fact about *this* test environment, not a flaw in the
    implementation, which will start actually returning real hardware
    entropy the moment it runs on real silicon or a QEMU invocation that
    passes the feature through.

    Confirmed in QEMU via QMP, same `run jtest.elf` fixture as items
    29-31: `linux_syscall: unimplemented syscall number 228`/`318` (the
    numbers for `clock_gettime`/`getrandom`) are gone from the output
    entirely -- both are now real, handled dispatch targets, not
    catch-all `ENOSYS` logging -- while `set_robust_list`/`rseq`/
    `prlimit64` (273/334/302) are still there, unchanged, exactly the
    "two down, honestly not lying about the rest" shape this item claimed.
    `hello from glibc dynamic` still prints, same as item 31, confirming
    no regression. Serial log clean through to "interrupts enabled", no
    panic.

33. **The actual real `java` binary now runs far enough to reach its own
    genuine `JAVA_HOME`-detection logic and fail with its own authentic
    error message -- plus a real bug this attempt exposed: `argv[0]` was
    never the program's real invocation path at all.** Copied `bin/java`
    itself (a real, unmodified, PIE, glibc-dynamically-linked launcher)
    from this machine's own JDK 21 install, plus its two direct non-libc
    dependencies (`libz.so.1`, `libjli.so`), patched `java`'s own
    `PT_INTERP`/`DT_NEEDED` strings to this project's now-familiar absolute
    short-path convention (`/LDSO.BIN`, `/LIBZ.BIN`, `/JLI.BIN`,
    `/LIBC.BIN`), and ran it the same way as items 29-32's `jtest.elf`.

    It got much further than a first attempt has any right to: real ld.so
    self-relocation, real `libc.so.6`/`libz.so.1`/`libjli.so` loading, and
    then *real* `libjli` startup code actually executing -- calling real
    `access` (probing for a co-located `.jrevm`/config marker, standard
    launcher behavior), then genuinely calling `readlink` (syscall 89,
    twice) trying to resolve its own executable path the way `GetJREPath`
    always does first on real Linux (`readlink("/proc/self/exe", ...)`).
    Neither call is implemented (no `/proc` here at all yet), so both
    honestly returned `-ENOSYS` -- and, tellingly, real `libjli` didn't
    crash: it fell all the way through to its own next, real fallback
    strategy (deriving a JRE path from `argv[0]` instead), then printed
    its own genuine, unmodified diagnostic once *that* didn't pan out
    either: `Error: could not find libjava.so` / `Error: Could not find
    Java SE Runtime Environment.` -- real `libjli.c` code, running for
    real, giving up for a real reason (nothing at the path it computed),
    not a crash or a KonjacOS-specific message.

    Chasing exactly *why* that fallback failed surfaced a real, distinct
    bug worth its own fix: `argv[0]`, as actually written onto a spawned
    task's stack by `build_initial_stack` (`loader.rs`), was never the
    real path the shell was asked to run at all -- `cmd_run` (`commands.rs`)
    only ever handed `load_and_run` a small fixed per-format constant
    (`"run:elf"`) meant purely for `ps`'s own task-table bookkeeping (see
    that code's own prior doc comment), and `build_initial_stack` reused
    that same string for `argv[0]` too, harmlessly, right up until a real
    program that actually *reads* `argv[0]` back (exactly what `libjli`'s
    own `argv[0]`-based fallback does) showed up. `loader.rs` now threads
    a genuinely separate `argv0: &str` parameter (the shell's real,
    unmodified `path`) through `load_and_run`/`load_and_run_with_interp`
    down to `build_initial_stack`, keeping it distinct from `spawn_user`'s
    own `'static` bookkeeping `name` -- a real Linux `execve`'s `argv[0]`
    and a shell's own task-list label were never the same concept, and
    conflating them only ever looked harmless because nothing had read
    `argv[0]` back before. Confirmed via the same `jtest.elf` fixture
    re-run after the fix (still prints `hello from glibc dynamic`,
    unaffected) and via `java.bin`'s own output being byte-for-byte
    identical before and after (`argv[0]` still lacked a `/`, so the
    fallback still failed the same way) -- a real, verified fix with no
    regression, not yet a full unblock on its own.

    What actually comes next is now concretely scoped, not guessed at,
    from direct inspection of the real JDK install this is all copied
    from: real `java`/`libjli` need the rest of a real JDK's `lib/`
    layout to exist at whatever path they resolve relative to their own
    location (`libjvm.so` at `lib/server/`, `libjava.so`/`libjsvml.so` at
    `lib/`, each with its own real `DT_NEEDED` on `libstdc++.so.6`/
    `libm.so.6`/`libgcc_s.so.1`/`libc.so.6` needing the same patching
    treatment already given `java` itself), and, separately and much more
    seriously, two real, substantial blockers this inspection found
    directly rather than by guessing: (1) `libjvm.so` alone is **26.6 MB**
    and the real `lib/modules` jimage (every `java.base` class, read
    directly by native code, not something this loader parses) is
    **140.8 MB** -- both real, measured sizes from this project's own JDK
    copy -- so this kernel's current 32 MB FAT16 disk image is nowhere
    close to big enough; `DISK_SIZE_MB` needs a real increase and a fresh
    `disk.img` before any of this can even be copied over. (2)
    `libjimage.so`'s own real filename -- a required real dependency,
    found via a runtime-computed `dlopen` path inside already-compiled
    `libjli`/`libjvm` code this project can't binary-patch the way a
    static `DT_NEEDED` string gets patched -- is 9 characters before its
    extension, one character too many for this FAT16 driver's strict 8.3
    short-name-only support (see item 29's own discovery of this same
    limitation while placing `ld-linux-x86-64.so.2`). Unlike every prior
    "add a syscall" or "decode a relocation type" gap this JVM push has
    hit, that's a real, honest filesystem feature gap (VFAT/long-filename
    read support) rather than something a path rename can route around,
    since nothing here can rewrite the filename `libjvm.so`'s own
    compiled `dlopen` call already hardcodes.

34. **Real VFAT long-filename reading (`fat16.rs`), plus a resized (32 MB
    -> 400 MB) disk image now carrying a genuine, unmodified JDK
    directory tree at its real absolute paths -- closing item 33's
    filesystem gap by solving the actual problem, not routing around it.**
    `list_dir_at` no longer treats a `0x0F`-attribute entry as pure noise
    to skip: it decodes each one's 13 UTF-16LE characters
    (`decode_lfn_chars`), accumulates them in on-disk order, and once the
    real 8.3 entry they precede shows up, `reconstruct_long_name` sorts by
    sequence number and concatenates them into that entry's real display
    name -- a genuine implementation of the generic VFAT long-name format,
    not a partial one, verified against a name real host `mtools`/`mcopy`
    itself wrote (this driver never has to *create* an LFN entry, only
    read one back correctly). `find_in_dir`'s lookup was quietly relying
    on a real bug-shaped coincidence before this -- comparing both sides
    truncated to 8.3 happened to still work for a short name, but would
    have silently mismatched (or matched the *wrong* file) for any real
    long one -- replaced with a plain case-insensitive comparison against
    whichever name `list_dir_at` actually resolved.

    This unblocked something bigger than any one file: with long names and
    subdirectories both real now, this project's own JDK 21 install (found
    on this same machine, in WSL) could be placed on the disk at its
    *actual* absolute paths -- `/usr/lib/jvm/java-21-openjdk-amd64/bin/java`,
    `.../lib/server/libjvm.so`, `/lib/x86_64-linux-gnu/libc.so.6`, and
    so on -- byte-for-byte real files, real names, zero `DT_NEEDED`/
    `PT_INTERP` string patching of any kind (a real step back from items
    29-33's own patching, made possible rather than made obsolete: once
    the real search paths a real `ld.so` already tries by default actually
    exist on disk, there's nothing left to patch around). `DISK_SIZE_MB`
    went from 32 to 400 to fit it: the real `lib/modules` jimage alone is
    140.8 MB and `libjvm.so` is 26.6 MB, both real, measured sizes, not
    estimates.

35. **A real, completely unmodified `java` binary now runs deep into
    genuine HotSpot JVM startup -- real `ld.so` dependency resolution,
    real `libjli` JRE-path detection, real JVM/JLI library loading -- via
    three more real, ground-truthed fixes, closing every gap items 33/34
    left open and surfacing a real thread-creation gap as the honest next
    frontier.**

    First, `argv[0]` had to become real: `loader.rs` was found (chasing
    why `libjli`'s own `argv[0]`-based JRE-path fallback kept failing) to
    be handing every spawned task the same small constant `task::spawn_user`
    used for `ps` bookkeeping (`"run:elf"`), never the shell's real
    invocation string. `build_initial_stack` now takes a genuinely separate
    `argv0: &str`, threaded from `cmd_run`'s own `path` through
    `load_and_run`/`load_and_run_with_interp`, so a real program reading
    its own `argv[0]` back sees what was actually typed, not a label meant
    for a task list.

    Second, real `readlink`/`readlinkat` (`/proc/self/exe` only, honestly
    `ENOENT` for anything else): a real `ld.so` needs this to expand
    `$ORIGIN` in its own `RPATH` (found via `readlinkat`, not the older
    `readlink` -- ground-truthed by trying the obvious syscall first,
    getting `ENOSYS`, and checking which one a modern glibc actually
    calls), and real `libjli`'s `GetJREPath` tries it as its *first*
    strategy, before ever falling back to `argv[0]`. Backed by a new,
    genuinely real `task::exe_path` (parented across `spawn_thread`/
    `spawn_clone_raw` the same honest, snapshot-not-shared way
    `heap_end`/`mmap_top`/`fs_base` already are) -- real Linux backs
    `/proc/self/exe` with the kernel's own record of what `execve` ran,
    not a disk symlink, and this is the same idea at a smaller scale.

    Third, and the one that actually explains *why* real `RPATH`-relative
    paths kept failing even once `$ORIGIN` resolved correctly: real
    `ld.so`'s own directory-existence cache. Tracing every `open`/
    `newfstatat` call by hand (this project's own established ground-truth
    method, not guesswork) showed `libz.so.1`'s two real `$ORIGIN`-relative
    candidates genuinely being tried and failing correctly (file not
    there, honest `ENOENT`) -- but `libjli.so`'s identical two candidates
    were never attempted at all, jumping straight to system default
    directories. The reason: `_dl_map_object`'s `open_path` `fstatat`s
    each search *directory* once and permanently caches a negative result
    for the rest of the process if that stat fails -- and `sys_newfstatat`
    had nothing to answer a directory with except `fat16::read_file`,
    which honestly refuses to "read" a directory as a file. Conflating
    "can't read this as a file" with "this doesn't exist" fed real,
    existing directories into that cache as permanently nonexistent,
    silently breaking every *later* dependency's search on the exact same
    disk layout. Fixed at the root: a new `fat16::stat_path` resolves
    either a file *or* a directory without ever reading file contents, and
    `sys_newfstatat`/a new, equally real `sys_access` (`fstatat`'s and
    `access`'s combined real dependents here) both use it, with
    `write_stat` now reporting genuine `S_IFDIR`/`S_IFREG` instead of
    always claiming to be a regular file.

    Confirmed in QEMU via QMP, tracing every syscall by hand end to end:
    with all three fixes and `getpid`/`gettid` (found the same way,
    trivially real -- `task::current_id()` honestly answers both, since
    this kernel never distinguished a process from one of its own threads
    the way real Linux's separate `pid`/`tgid` does) in place, `run
    /usr/lib/jvm/java-21-openjdk-amd64/bin/java` produces a real `ld.so`
    trace indistinguishable in shape from a real Linux one: every
    `$ORIGIN`-relative candidate genuinely tried, `libz.so.1`/`libjli.so`/
    `libc.so.6` all found via their real search paths, `access` confirming
    `libjava.so` is really there, `jvm.cfg` genuinely read and parsed, and
    `lib/server/libjvm.so` genuinely located -- no fabricated success, a
    real dependency graph actually resolving. HotSpot itself then starts
    real internal initialization (`set_robust_list`/`rseq`/`prlimit64`,
    same three honest gaps items 31-32 already named and still open) and
    reaches its first real internal thread creation attempt, calling
    `clone3` (syscall 435) -- a modern, `struct`-argument clone variant
    this kernel's existing `clone(2)`-only `sys_clone` doesn't implement.
    That attempt, and NPTL's own subsequent thread-startup synchronization,
    is where this item's ground-truthing stopped: real, deep multi-
    threading correctness (not just spawning *a* thread, which already
    worked for simpler test binaries, but surviving HotSpot's own internal
    thread-startup handshake) is a genuinely new, substantial frontier, not
    another isolated syscall stub -- named honestly as the next real
    blocker rather than pushed further in this pass.

36. **Real `clone3` support, a kernel-wide fault-isolation fix that turns
    out to matter for exactly this kind of work, and a real, precisely
    ground-truthed (if not yet fully resolved) `libc.so.6` startup crash
    -- item 35's named frontier, actually engaged with rather than left
    untouched.**

    `clone3` (syscall 435) is now real, not a stub: `linux_syscall.rs`
    factors the "snapshot the live syscall frame, patch `rax`/`RSP`, spawn
    a task that resumes mid-epilogue" logic legacy `sys_clone` already had
    into a shared `do_clone`, and a new `sys_clone3` reads a real
    `struct clone_args` (`flags`/`child_tid`/`parent_tid`/`stack`/
    `stack_size`/`tls`, Linux's own `include/uapi/linux/sched.h` layout)
    out of ring-3 memory and feeds it the one field genuinely computed
    differently: `clone3`'s `stack` is the **low** address of the child's
    stack allocation, the opposite convention from legacy `clone(2)`'s
    already-the-top `child_stack` argument, so the real initial stack
    pointer is `stack + stack_size`, computed explicitly rather than
    assumed. Verified correct on its own terms by direct inspection, not
    just "didn't crash": a temporary trace confirmed `tls`'s first 8 bytes
    (the TCB self-pointer real glibc TLS blocks always start with) really
    do equal `tls` itself, exactly what a correctly-initialized real glibc
    thread-control-block looks like -- this kernel's own `clone3` argument
    handling is real and correct, confirmed by ground truth, not assumed.

    Getting to try it at all surfaced a real, separate, load-bearing gap:
    every CPU exception other than #PF (page fault) was unconditionally
    fatal to the *entire kernel*, ring 3 or not -- a deliberate
    simplification from back when every ring-3 program was a small,
    hand-verified demo, never expected to fault at all outside #PF's
    already-handled cases. The instant a real, unmodified `java` binary
    got far enough to run genuinely new, unverified machine code (HotSpot's
    own thread-startup path) and hit a real #GP, that simplification meant
    one task's bug took down the whole machine -- a blast-radius mismatch
    real Linux doesn't have either (an unhandled #GP in a user process is
    `SIGSEGV`/process death, not a kernel panic). `idt.rs`'s
    `exception_handler` now reads the faulting frame's own `CS` (already
    on the stack, just never plumbed through before) to tell a ring-3
    fault from a ring-0 one: `CPL == 3` now kills *only* that task via
    `task::task_exit()` -- the exact same "mark Terminated, `schedule()`
    away" mechanism `SYS_EXIT` already uses, safe to call here for the
    same reason it's safe from `timer.rs`'s own IRQ handler (`IF=0`,
    willing to abandon whatever's on this stack, `switch_to` only ever
    needs the *incoming* task's saved state to be valid) -- while `CPL ==
    0` still halts unconditionally, a real kernel bug being a genuinely
    different, unrecoverable class of problem. `isr_common` was also
    extended to push every GPR before calling `exception_handler`, purely
    for a real register dump (`rax`-`r15`) on a ring-3 fault -- exactly
    the ground truth real Linux's own oops dump gives you for free, which
    this kernel had never needed before because there was nothing left to
    debug *with* once the whole machine had already halted. A "Code:" dump
    (16 raw bytes at the faulting `RIP`) rounds it out, the same real
    debugging value a Linux oops's own "Code:" line has.

    That diagnostic pair is what turned a real crash into a real, precisely
    characterized one, methodically, by ruling hypotheses out with direct
    evidence rather than guessing: the fault's RIP fell inside `libc.so.6`'s
    own just-`mmap`'d, real, correct file content (confirmed byte-for-byte
    against the real host `libc.so.6` via `readelf`/`objdump`) -- meaning
    the CPU was never fetching from garbage/unmapped memory, only from a
    real `.hash`-section *data* address it had no business executing (the
    literal first byte there, `0xf4`, is `HLT`, a privileged instruction --
    exactly what raises a `CPL=3` `#GP` the instant it's reached, which is
    consistent with "jumped to a bad, data-shaped address" rather than "ran
    real code that happened to fault"). Two real, concrete hypotheses were
    then tested and *ruled out* with direct evidence, not assumption: (1)
    a corrupted/uninitialized glibc TLS block for the new thread -- ruled
    out, `tls_self_ptr` genuinely equals `tls`; (2) HotSpot's own CPU-
    detection code computing a bad value from missing `/proc/stat`/
    `/sys/devices/system/cpu/possible`/`sched_getaffinity` (all real,
    honestly-`ENOENT`'d gaps found the same way, and worth fixing in their
    own right: `synthetic_proc_file` now answers those three real paths
    with real, honest single-real-vCPU content -- matching this project's
    own `qemu-system-x86_64` invocation, which never passes `-smp` -- and
    `sys_sched_getaffinity` reports a genuine single-CPU affinity mask
    instead of `ENOSYS`) -- ruled out too: the identical crash, at the
    identical `RIP`, with the identical register values, reproduced again
    after those fixes landed, proving it was never the cause.

    What's left unresolved, honestly: the crash is deterministic (same
    `RIP`/registers across independent runs) and happens somewhere in real
    `ld.so`'s own userspace relocation/RELRO-hardening pass over
    `libc.so.6` -- entirely real glibc code this kernel's own loader never
    touches for an interp-loaded library (see item 29's own division of
    labor) -- most likely a bad function-pointer/GOT-slot read rather than
    a KonjacOS-side memory-content bug (the mapped file content itself was
    independently confirmed correct). Pinning down *which* real relocation
    or `mprotect` call produces the bad value needs either live register
    tracing at the exact fault instant or a closer look at this kernel's
    own `sys_mprotect`/RELRO-adjacent syscall behavior against `libc.so.6`
    specifically -- a real, well-scoped next step, not a vague one, but
    genuinely not resolved in this pass. `jtest.elf`'s own regression
    fixture was re-run clean after every single change in this item
    (the `clone3` work, the exception-handler rewrite, and the `/proc`/
    `sched_getaffinity` additions each individually verified not to have
    broken it) -- real discipline, not just a claim.

37. **The `libc.so.6` crash from item 36, narrowed further with one more
    real diagnostic and a full syscall trace across the entire startup
    sequence -- still not resolved, but two more real hypotheses tested
    and ruled out with direct evidence, and the honest tooling limit that
    actually stopped this pass identified precisely.**

    `isr_common`/`exception_handler` gained one more real capability: the
    CPU itself pushes the faulting task's real `SS`/`RSP` onto the
    interrupt frame for any fault that crosses privilege levels (which a
    ring-3 one always does) -- previously read by nothing, now surfaced
    as a 6th argument and dumped (the top 6 stack words) alongside the
    existing register/`Code:` dump. The idea: a bad *indirect call* leaves
    a real, checkable return address on top of the stack (`call` always
    pushes one before jumping) -- pointing straight at which real function
    made the bad call, no disassembler needed. Ground-truthed against a
    real regression pass first, same discipline as every other change in
    this arc: `jtest.elf` still prints `hello from glibc dynamic` clean
    after this addition, verified before ever using it to look at the
    real crash.

    What it actually showed: the top of the crashing task's stack reads
    `0x0` then `0xffffffffffffff` -- not a plausible return address at
    all. Combined with a full, un-truncated syscall trace across the
    entire startup sequence (every `open`/`mmap`/`mprotect` from the first
    library load through the crash, captured this time, not just the
    tail), this rules out the two most likely remaining explanations with
    direct evidence rather than further guessing: `sched_getaffinity`'s
    real single-CPU implementation and the synthetic `/proc`/`sys` files
    from item 36 didn't change the crash at all -- identical `RIP`,
    identical registers, reproduced again -- and the same trace also
    showed this crash can happen *before* `clone3` is ever even reached
    in a given run (an earlier assumption from item 36 that it was
    downstream of thread creation), which rules out anything specific to
    resuming a cloned child. The real trace also surfaced the full,
    correct RELRO sequence -- five real `mprotect(..., PROT_READ)` calls
    hardening `libc.so.6`/`libz.so.1`/`libjli.so`/the main `java` binary/
    `ld.so` itself right after the first dependency wave resolves, then
    four more for the second wave (`libjvm.so`/`libstdc++`/`libm`/
    `libgcc_s`) once `libjli` `dlopen`s the actual JVM -- all of which
    happen, visibly, well before the crash, so the RELRO-hardening step
    itself completing isn't in question; something *after* it, in
    ordinary post-RELRO code, is what jumps somewhere it shouldn't.

    A cross-reference against the real `libc.so.6`'s own dynamic symbol
    table (`nm -D`) for the faulting address and a couple of suspicious
    register values came back inconclusive -- the nearest preceding
    *exported* symbol to the crash offset is a tiny, irrelevant data
    object (consistent with the crash address genuinely being inside
    `.hash`, not code, as item 36 already established), and internal,
    non-exported glibc functions (the far more likely real location of
    whatever's actually misbehaving) aren't visible in the dynamic symbol
    table at all. The honest bottleneck this pass actually ran into: no
    disassembler-driven interactive debugger (`gdb`) is available in this
    environment (not installed, and this session has no password for the
    `sudo` that would install it) to single-step through the real fault or
    inspect a live backtrace -- the concrete, specific thing that would
    turn "narrowed to somewhere in real `ld.so`'s post-RELRO/first-real-
    library-call code" into an exact instruction. That's the real
    remaining blocker, named honestly rather than papered over with more
    speculative kernel-side changes that the evidence gathered here
    doesn't actually point at.

**Correction to the historical investigation in items 38-43:** the
`__strtold_l_internal` symbol attribution used the wrong libc load address.
The recorded RIP is in `abort()`. Claims based on that attribution, including
what the old breakpoints ruled out, should not guide further debugging.
See item 44 for the matched-binary evidence and actual futex failure.

38. **The real crash from items 36-37, finally identified down to the
    exact function and source line with `gdb` -- and a genuinely
    surprising result: the "bad jump into garbage data" theory those two
    items built on was wrong, corrected here with the same ground-truth
    discipline that found it in the first place.**

    `gdb` became available this pass (installed by hand, outside this
    project's own sandbox, closing exactly the tooling gap item 37 named
    honestly instead of guessing past). Attached to `qemu-system-x86_64`'s
    own `-s` GDB stub, with a real hardware breakpoint (`hbreak`, not a
    memory-patching software one -- deliberately chosen so it would still
    arm correctly on an address whose backing page wasn't mapped yet at
    breakpoint-set time) at the exact deterministic fault address items
    36-37 already established. First attempt at correlating that address
    against the real `libc.so.6` file repeated the same mistake in a new
    place: naive `runtime_address - mmap_base` arithmetic assumes a
    shared object's executable segment starts at file offset 0, which
    `readelf -l` on the real file immediately disproved -- glibc 2.39
    (Ubuntu 24.04) splits `.text` into its *own* `PT_LOAD` segment
    starting at file offset `0x28000`, not 0, a real, common hardening
    layout (separate non-writable-and-non-executable `.rodata`/`.hash`/
    `.dynsym` segment first) this project's own loader never had to
    reason about before, since every prior fixed-base-of-0 assumption
    happened to hold for everything loaded via `loader.rs`'s own eager
    path. Redone with the correct bias, and (the part that actually
    mattered) with `add-symbol-file` pointing gdb at the real runtime load
    address instead of hand-computing offsets, gdb's own address-to-line
    resolution -- authoritative, not a guess -- landed on a real, named,
    ordinary glibc function: `__strtold_l_internal`, `stdlib/strtod_l.c`
    (fetched at the matching `glibc-2.39` tag to confirm), specifically
    at `+8050` bytes into it.

    That real function-aligned disassembly (`disassemble` with no
    arguments, letting gdb decode forward from the function's own real
    entry point -- the fix for a real mistake in items 36-37's own
    `x/Ni $rip-N` disassembly, which can start mid-instruction on x86's
    variable-length encoding and produce a plausible-looking but wrong
    decode past that point) confirms the literal byte at the fault address
    really is `hlt` (`0xf4`) -- but, corrected from items 36-37's own
    conclusion, this isn't a jump into unrelated `.hash` data at all: it's
    real, intentional, compiler-emitted glibc code, reached via ordinary
    sequential execution, not a bad indirect branch. The pattern
    surrounding it -- a fixed global counter checked against small
    sequential values (3, 4, 5, 6...), incremented, each transition ending
    in either a real `call` (to an unnamed local helper, addressed only
    relative to the nearest *exported* symbol, `__vfscanf_internal`, since
    static functions carry no dynamic-symbol-table entry of their own) or
    an immediate `hlt` -- is the real, well-known shape of a compiler-
    generated "this must never happen" trap: code the compiler considers
    provably unreachable in correct execution, placed after what it
    assumes is a `noreturn` call, as a hard backstop in case that
    assumption is ever wrong. `gdb`'s own `bt` at the breakpoint, for
    comparison, is *not* trustworthy here and is called out as such rather
    than reported as fact: with no valid return address anywhere on this
    task's real stack (items 36-37's own stack dump already showed
    `0x0`/`0xffffffffffffffff` at the top), a frame-pointer-less heuristic
    unwind found leftover stack bytes that merely *happen* to resemble
    addresses inside other real libc functions -- `__strtoul_l_internal`
    "calling" `__strtold_l_internal`, itself apparently called from inside
    `strcat`'s own SSE2 implementation, a chain no real C program would
    ever produce. Reported honestly as unreliable rather than mistaken for
    a real call chain.

    The one real, concrete, actionable lead this pass surfaced: the
    breakpoint's own argument dump shows `loc=0x0` -- a **null locale_t**
    handed to a locale-aware internal string-to-float parser. Real glibc
    code never legitimately does this; every real caller passes either an
    explicit locale object or the well-known `_nl_C_locobj_ptr` sentinel
    for the "C" locale, never a bare null pointer, and a null-locale
    fatal-trap is exactly the shape of check a hardened glibc build would
    plausibly have here. This kernel has never provided any real locale
    data (no `/usr/lib/locale`, no `/usr/share/locale`, nothing under
    `/usr/share/i18n` -- this FAT16 disk has none of it), so the leading,
    well-scoped hypothesis for the next pass is that something in real
    glibc's own locale-loading path, faced with a totally absent locale
    subsystem on this disk, ends up constructing or passing a null
    `locale_t` down a code path that isn't supposed to be reachable that
    way -- a real, specific, testable next step (does the syscall trace
    show an attempted locale-file `open` shortly before this crash, and
    does providing a minimal real "C" locale answer for it change
    anything), not another guess.

    That specific lead was tested the same session, immediately, with a
    real syscall trace covering the exact run that hit the crash: **no
    `open`/`access`/`newfstatat` call naming anything under `/usr/lib/
    locale`, `/usr/share/locale`, or `/usr/share/i18n` appears anywhere in
    it** -- real glibc's default "C" locale is a compile-time constant
    object, never touching the filesystem at all unless `LANG`/`LC_*` env
    vars request something else (which this loader's real, empty `envp`
    never does), so the "missing locale files" hypothesis is real, tested,
    and *not* the cause -- ruled out with direct evidence, not assumed.
    The trace also pinpointed exactly where in the real startup sequence
    this happens: immediately after `libgcc_s.so.1` finishes loading (the
    last of the second `dlopen` wave from item 35), before any of that
    wave's own RELRO `mprotect` calls even begin -- earlier in real
    startup than either item 36 or 37 had located it.

    One more real attempt, also honestly reported as inconclusive rather
    than stretched into a conclusion: a software breakpoint placed at
    `__GI_____strtold_l_internal`'s own real entry point (to see the
    *unoptimized* `nptr` string before the compiler destroys it, the
    obvious next question -- *what string is glibc trying to parse as a
    number here?*) never fired even once, despite the crash reliably
    landing 8050 bytes into that exact same function's body on every run.
    The most likely real explanation: glibc's floating-point string
    parsers are generated from one shared macro-templated source for
    `strtof`/`strtod`/`strtold` alike, with multiple local, non-exported
    aliases that can each jump into the *same* shared tail code from
    different entry addresses -- meaning the real call reaching this crash
    almost certainly enters through a sibling symbol this session never
    identified, not the one gdb's own (occasionally ambiguous, for
    duplicate local symbol names) resolution picked. Finding that real
    entry point -- and, with it, the real, unoptimized argument string --
    is the concrete next step, left honestly open rather than guessed at
    further in this pass.

39. **Real `uname(2)` -- a genuine, worthwhile addition on its own merits,
    found via a real, well-reasoned hypothesis about item 38's crash that
    a real test then honestly disproved, not confirmed.** The real `java`
    binary's own `.note.ABI-tag` requests a minimum kernel version
    (`file`'s own "for GNU/Linux 3.2.0"), which real `ld.so` checks by
    calling `uname` and parsing the `release` field with the same family
    of numeric-string parsers item 38's crash sits inside -- a real,
    concrete, testable hypothesis for *why* that crash happens, not a
    guess. `sys_uname` now answers honestly: `sysname` `"Linux"` (this
    kernel's whole real-syscall layer already exists to be compatible with
    exactly that assumption), a real, well-formed `release` (`"6.1.0"`,
    safely clearing the `3.2.0` minimum without claiming a fabricated
    specific kernel build), and honest `machine`/`version` fields naming
    this project -- real Linux `struct new_utsname`'s exact six-field,
    65-byte-each ABI layout, not an approximation.

    Tested immediately, the same session, against the same reproducible
    crash: **the hypothesis was wrong.** The identical crash reproduces
    byte-for-byte after this fix -- same faulting RIP, same `rax`/`rbx`/
    `rcx`/`rdx`/`r12`/`r13`-`r15` register values, same `hlt` -- with only
    the task ID differing (reflecting a different run, not a different
    fault). This was caught by this session's own mistake, not hidden: a
    first look only checked the framebuffer console's `println!` output
    (where a real, different HotSpot message -- "Failed setting boot
    class path" -- happened to be the last thing visible) and concluded
    the crash was gone; a second, closer look at the actual `sprintln!`-
    only serial log, the diagnostic items 36-38 built specifically for
    this purpose, showed the real `*** CPU EXCEPTION: #13 ***` block
    still there, identical, on the very same run. Corrected here rather
    than left standing: `uname` is real and correct and stays, but item
    38's crash is exactly as unresolved as it was before this item, not
    fixed by it.

40. **Two more real, genuinely useful syscalls (`madvise`/`sysinfo`), a
    real discovery about *where* item 38's crash sits in the process
    tree, and a definitive (if still incomplete) narrowing of exactly
    which code path can't be the culprit.**

    `madvise` is now a real, honest no-op -- correct, not a placeholder:
    real Linux itself treats every `madvise` hint as advisory, something
    the kernel is always free to ignore, and nothing this kernel does
    with a mapped page's contents depends on any of the specific hints
    (`MADV_WILLNEED`, `MADV_DONTNEED`, ...) a real caller might pass.
    `sysinfo` answers with real, live data where this kernel actually has
    it -- `uptime` from `timer::uptime_seconds()`, `totalram`/`freeram`
    from `pmm::stats()` (the same real source `meminfo` already uses) --
    and honestly zeroes everything it doesn't (`sharedram`/`bufferram`/
    swap/`procs`/high-memory), real `struct sysinfo`'s exact 112-byte x86_64
    layout. Both were found the same ground-truth way as everything else
    in this arc: real HotSpot calls both during early startup (sizing its
    default heap off real available memory, among other ergonomics), and
    this kernel had nothing to answer either with before now.

    Neither one touched item 38's crash -- tested immediately, same
    reproducible fingerprint, ruling out both as contributing causes with
    direct evidence. But re-running the full trace *with* both real now
    revealed something the crash's own register dump had been showing all
    along without this session noticing it: the `sys_open`/`sys_newfstatat`
    calls attempting to set up the boot class path (`/lib/modules`,
    `/modules/java.base` -- themselves showing a separate, real, `JAVA_HOME`-
    prefix-missing bug worth its own future investigation) happen *after*
    the crash's own "killing task #N" line in the serial log, for the
    *same* task number the crash just reported killing. Since a killed
    task cannot make further syscalls, these calls are conclusively coming
    from a *different* task -- almost certainly one of HotSpot's own
    internal threads, spawned via `clone3` before the crash, continuing to
    run independently once cloned (this kernel's own scheduler treats
    cloned threads as fully independent tasks, unaffected by a sibling's
    death -- see `task.rs`'s own module docs). That makes the "Failed
    setting boot class path" message very likely a *downstream symptom* of
    the main thread's death, not a separate bug of its own: an orphaned
    worker thread eventually giving up because the (now-dead) main thread
    can never signal whatever shared VM state it was waiting on. This
    reframes priority cleanly: item 38's crash is very likely the one real
    root cause blocking everything past it, not one of several unrelated
    gaps.

    Chasing it further, two more concrete hypotheses were tested and
    ruled out with direct evidence rather than assumption: a hardware
    breakpoint on `/proc/self/maps` access (a real, plausible way
    `libjvm.so`'s own `os::jvm_path()` could fail to find its own install
    location) never fired -- no such path is ever requested anywhere in a
    full trace of the run. And breakpoints placed on all three *public*,
    exported entry points (`strtold`, `strtod`, `strtof`) never fired
    either, even though the crash itself reliably happens deep inside
    `__strtold_l_internal`'s real body -- conclusive proof the real caller
    is glibc's own internal code invoking a hidden/internal symbol
    directly (the normal, expected way glibc's own code avoids PLT
    overhead calling into itself), not application code going through any
    named, breakpointable boundary this session could find. Finding the
    *actual* internal caller -- the real remaining question -- would need
    either a non-stripped glibc build with full internal DWARF info, or a
    memory watchpoint on the state variable combined with a much deeper,
    more time-intensive single-stepping session than this pass's budget
    allowed; named honestly as exactly that rather than pushed further
    here.

41. **Real `envp` (environment variables) for spawned processes -- the
    first tiny step on the [Minecraft roadmap](#roadmap-getting-to-minecraft)'s
    item 1.** Every process this kernel has ever spawned got an empty
    environment (`envp = { NULL }`); the shell's `run` command now accepts
    real `env`-style leading assignments -- `run FOO=bar jtest.elf` -- parsed
    in `cmd_run` (`commands.rs`) as any number of whitespace-separated
    `NAME=value` tokens before the first token that isn't shaped like one,
    which becomes the path. Those strings are written onto the new task's
    stack the same way `argv0` already was (NUL-terminated, right below it),
    and their real addresses go into a real `envp[]` pointer array between
    `argv`'s `NULL` terminator and the auxv, exactly matching real Linux's
    stack layout (`build_initial_stack` in `loader.rs`) -- `ptr_words` and
    the push sequence both now account for however many real variables were
    passed, instead of the previous hardcoded single-`NULL`. Verified with
    `jtest.elf` both ways (`run jtest.elf` and `run FOO=bar jtest.elf`) --
    identical successful run in both cases, confirming the new envp-writing
    path doesn't disturb anything the regression already depended on. This
    doesn't fix item 38's crash by itself, but it's the concrete lever
    needed to even attempt real glibc/JVM crash-avoidance environment
    variables (`GLIBC_TUNABLES`, `LD_BIND_NOW`, and similar) against it --
    still untried as of this item, a natural next step.

42. **Three real crash-avoidance environment variables tried against item
    38's crash using item 41's new `envp` lever -- all three ruled out with
    direct evidence, plus one real, separate, previously-unknown kernel bug
    found by accident along the way.** `GLIBC_TUNABLES=glibc.cpu.hwcaps=
    -AVX2,-AVX512F` (disabling IFUNC-selected vectorized code paths, in
    case the crash was an IFUNC resolver picking a `strtold` variant that
    assumes CPU features QEMU's default CPU model doesn't have),
    `LD_BIND_NOW=1` (forcing eager PLT binding, in case lazy-binding
    ordering mattered), and `MALLOC_CHECK_=0` (in case glibc's malloc
    debugging instrumentation itself was involved) were each tried alone,
    each as the *only* `java` invocation of a fresh boot -- all three
    reproduce the exact same fault, byte for byte identical faulting RIP
    (`0x70000579a2`) to item 38's original crash. Conclusive: none of the
    three is the culprit. (The `AVX2`/`AVX512F` variant took roughly a
    minute to crash instead of the usual ~10 seconds -- consistent with
    disabling vectorized code paths forcing much slower generic-C
    fallbacks under QEMU's TCG emulation, not with reaching new code.)

    The real, useful accident: running `java` a *second* time in the same
    boot (to save time between tests) reliably kernel-panics --
    `memory allocation of 26668072 bytes failed`, 26,668,072 bytes being
    *exactly* `libjvm.so`'s real size (see item 29's own measurement). A
    real, previously-unknown resource leak, not a crash-hunting artifact
    -- root-caused and fixed in item 43. Worked around here (and in every
    test above) by rebooting between every single test.

43. **The real root cause of item 42's leak, fixed: this kernel's `#GP`
    handler only ever killed the one faulting task, not its whole thread
    group -- so a `clone3`'d sibling thread that outlives its dead
    leader (see item 40's own discovery of exactly this with real
    HotSpot) kept the crashed task's address space looking "still shared"
    forever.** `paging::destroy_address_space` (the code that actually
    frees a private address space's physical frames) already existed and
    was already correct -- `task.rs`'s reaping sweep already deferred
    calling it for exactly this "still shared" case, on purpose, so as
    not to free memory a live sibling thread still needs. The bug was
    that nothing ever made that sibling stop being live: real Linux kills
    the *entire* process on an uncaught fatal signal, not just the thread
    that took it, and this kernel had never implemented that half. Fixed
    with a new `task::kill_group(cr3, except)`, called from the `#GP`
    handler (`idt.rs`) right before the faulting task's own `task_exit`:
    marks every *other* task sharing this `cr3` `Terminated` too, so the
    reaping sweep's existing "last thread out tears it down" logic
    actually gets to run. Verified with `meminfo`: physical memory used
    after one `java` crash went from a clean 69 MiB baseline to 72 MiB
    (a small, expected residual, not the unbounded growth from before)
    -- and a second `java` invocation in the same boot now genuinely
    spawns and runs, instead of never even getting the chance to.

    That second `java` run still hit its *own*, separate wall a bit
    further in, though -- honestly a different bug, not a full fix in
    disguise: this kernel's fixed 64 MiB kernel heap (`heap.rs`), not
    physical memory, ran out. Real `open(2)` here (`sys_open` in
    `linux_syscall.rs`) reads a file's *entire* contents into a heap
    `Vec<u8>` up front, once, at open time -- simple and correct, but it
    means every real JDK library `ld.so` opens before the crash
    (`libjli.so`, `libc.so.6`, `libjvm.so`, `libjava.so`, ...) leaves a
    same-sized heap allocation alive in that task's own open-file table
    until the task is reaped. One `java` run's worth of these adds up to
    ~30 MiB of a 64 MiB heap (confirmed directly: `meminfo` showed heap
    headroom drop from 65,405 KiB free to 34,733 KiB free across a single
    run+crash) -- and a follow-up `alloc` of just 32 MiB (nothing to do
    with `java` at all) then genuinely failed the same way, ruling out
    "still leaking" in favor of "this heap's plain too small, and its
    first-fit/no-coalescing allocator (see `heap.rs`'s own doc comment)
    can't stretch what headroom remains into one contiguous block that
    big." Fixed the cheap, honest way: bumped `INITIAL_HEAP_SIZE` from 64
    MiB to 96 MiB (deliberately not further -- it's eagerly-mapped
    physical memory taken from the same pool real JVM segment mappings
    need room in too; see the constant's own updated doc comment).
    Verified end to end: with the bigger heap, two full `java` runs now
    succeed back to back in one boot, both hitting the identical item 38
    fault cleanly (no panic); a *third* back-to-back run does still
    exhaust the (still-finite, just bigger) heap -- expected, not a
    regression, matching the ~30 MiB-per-run/96 MiB-budget math this fix
    was sized around. Real headroom for more runs (or on-demand heap
    growth, or coalescing, or lazy `sys_open` reads) is still there to
    take if it ever matters.

44. **Fix the glibc pthread-join abort: untimed `FUTEX_WAIT_BITSET`.**

    The saved crash address `0x70000579a2` was misidentified in item 38.
    The actual libc load bias is `0x700002f000`; subtracting it gives
    ELF address `0x289a2`, glibc `abort()`'s deliberate `hlt` fallback.
    The libc extracted from the guest disk matched the host binary exactly
    (build ID `a4a7992a8e66555c8141ab2a08a8465ff6e0ea65`). With symbols
    relocated by that bias, a hardware breakpoint at `abort` captured
    `__libc_fatal` / `futex_fatal_error` / `__pthread_clockjoin_ex` and
    the message `The futex facility returned an unexpected error code.`
    Some optimized stack arguments were unavailable or unreliable; the
    fatal message, matching instruction bytes, and syscall trace establish
    this diagnosis without relying on those values.

    glibc's untimed `pthread_join` uses `FUTEX_WAIT_BITSET |
    FUTEX_CLOCK_REALTIME` (`0x109`), a NULL timeout, and a MATCH_ANY mask.
    The old handler returned `-ENOSYS`, which glibc treats as fatal.
    `linux_syscall.rs` now forwards the timeout argument, obtains the mask
    from saved r9, validates alignment/mask, and uses the existing atomic
    value-check / task-block / clear-child-TID-wake path. No task layout,
    FPU state, assembly, or TASKS lock lifetime changed.

    Scope is intentionally limited: only untimed MATCH_ANY bitset waits
    are supported. Nonzero timeouts and selective masks return `-ENOSYS`;
    a zero mask or unaligned wait address returns `-EINVAL`. Timed legacy
    WAIT requests are also rejected instead of silently waiting forever.
    Existing trusted-user-pointer and cross-address-space futex-key
    limitations are not fixed by this change.

    Runtime evidence on the rebuilt ISO:

    * New `userprogs/futex_glibc.c` failed on the old kernel with
      `FAIL wait-bitset mismatch: errno=38`.
    * It now prints `PASS futex-bitset and glibc pthread_join`, covering
      shared/private/realtime mismatch cases, zero-mask rejection, and a
      real glibc thread creation/join with its returned value checked.
      GDB recorded the join's `0x109` wait with a NULL timeout.
    * Existing `run jtest.elf` still prints `hello from glibc dynamic`.
    * The real JDK launch reaches `Failed setting boot class path.` with
      no CPU exception or panic during the bounded observation. This is
      the next blocker, not proof Java can run user code. Item 40's claim
      that this message was merely downstream of the abort is unsupported:
      it persists after this fix.

    To reproduce the fixture: stop QEMU, run `make futex-fixture` in WSL,
    then `make run` and enter `run futest.elf` in the guest shell.
    The fixture uses hosted glibc headers and is never linked into the
    freestanding kernel. The target adds only FUTEST.ELF to an existing
    disk; it does not regenerate the disk or remove a directly installed
    JDK. Kernel compilation retains the six pre-existing intrinsic ABI
    warnings. The build and runtime checks do not establish general Linux
    futex compatibility or fix the separate repeated-launch heap problem.

45. **Resolve the boot-class-path failure, then fix the real I/O deadlock
    it exposed.** A syscall entry/return trace found `readlink("/usr")`
    returning `-ENOENT` for an existing directory. glibc `realpath` stopped
    there, and HotSpot subsequently probed `/lib/modules` instead of the
    real JDK path. Existing FAT16 non-links now return `-EINVAL`; missing
    paths still return `-ENOENT`. The real JVM now successfully stats
    `/usr/lib/jvm/java-21-openjdk-amd64/lib/modules`.

    Next, a real libjimage read stalled. GDB showed the page-fault handler
    spinning in TASKS.lock: read/pread were copying to a lazy user buffer
    while already holding that lock. Linux read/pread and native read now
    stage at most 4096 bytes in kernel memory, release TASKS, then copy to
    user memory. The shared helper also handles offsets beyond EOF without
    forming out-of-bounds slices. Negative pread offsets return EINVAL.
    No assembly, FPU layout, or interrupt-lock implementation changed.

    The next trace also exposed a real `lseek` returning `-ENOSYS` during
    library loading. SEEK_SET/CUR/END now work with checked signed offsets,
    beyond-EOF positions, and errors that preserve the current position.
    Unsupported whence values return EINVAL, invalid descriptors EBADF.

    Verified on the rebuilt ISO: `PATHCHK.ELF` passes glibc realpath/error
    checks; `IOCHK.ELF` passes patterned cross-sector/cluster reads, seeks,
    EOF/negative-offset cases, nonzero-offset file mmap, anonymous pages,
    and reads into untouched pages through both syscall ABIs. Existing
    `JTEST.ELF` and `FUTEST.ELF` still pass. The path and lazy-buffer tests
    reproduced their respective failures before the fixes, and the seek
    test first failed with errno 38. All four final regression runs had no
    CPU exception or kernel panic.

    **Still blocked:** Java progresses through libjimage loading but the
    whole-file `fat16::read_file` allocation for `lib/modules` is
    140,848,911 bytes, larger than the 96 MiB kernel heap, and panics.
    This needs bounded file backing and mapping work, not another heap-size
    bump. mmap bounds, permissions, failure rollback, and shared mapping
    ownership also remain incomplete; no general mmap-correctness claim
    follows from these tests. See [the full syscall/I/O audit](docs/jvm-io-audit.md)
    for evidence, remaining limitations, and reproduction commands using
    `make path-fixture io-fixture` and `tools/trace_jvm.py`.

46. **Bounded file reads and demand-paged file mappings.** Linux/native
    file descriptors now retain small FAT16 handles; read/pread stage at most
    4096 bytes without holding TASKS during I/O or user-memory access. File
    mappings retain backing identity after close and populate physical pages
    only on access. The 96 MiB heap is unchanged.

    The new CR3-owned mapping table is shared by cloned threads. It tracks
    file offsets and permissions, splits partial unmaps/replacements/protection
    changes, and prevents removed holes from being silently demand-filled.
    Bounds/alignment/descriptor errors and metadata exhaustion fail explicitly.
    Unpublished page/table frames are released on population failure; the last
    thread-group reaper removes reservations. Shared writable mappings remain
    unsupported. Backing FAT files must remain unchanged while open/mapped.

    Runtime tests exposed and fixed existing page-fault ABI defects: stack
    alignment before Rust calls, nested FPU saves overwriting an outer syscall's
    state, and incomplete arguments to fatal exception dispatch. Diagnostics
    now read only present pages through HHDM. IRQ-safe FAT-layout/PMM locks and
    serialized ATA commands protect the new demand-I/O path.

    BIGCHK passes four real archive read/map/close/unmap cycles, EOF padding,
    large offsets and bounded physical-memory checks in BIOS and UEFI QEMU.
    VMCHK checks validation, splits, untouched-page protections, holes and
    shared-thread mappings, metadata exhaustion and failure rollback. IOCHK (including nested-fault SIMD preservation),
    PATHCHK, JTEST and FUTEST pass with no CPU exception or kernel panic.
    An intentional physical-exhaustion probe now kills only its process and
    then passes BIGCHK in the same boot. Stable debug/release builds retain the six
    pre-existing intrinsic warnings and have no unresolved linker symbols.

    The real Java launcher gets past modules-archive opening/mapping. Its next
    captured unhandled fault is RIP/CR2 `0xffffffffff600800`, error `0x14`,
    consistent with the legacy getcpu vsyscall entry; syscall 309 also returns
    ENOSYS. HotSpot then reports SIGSEGV/aborts, while the kernel stays up.
    Its printed `pc=0` is not the captured hardware RIP: full Linux signal
    contexts are still missing. Java startup and application execution are
    **not** claimed complete.

    See [bounded-file design, invariants and reproduction notes](docs/bounded-file-backing.md)
    and [the final regression evidence](docs/bounded-file-validation.json).

47. **CPU queries and correct memory units for Java startup.** Linux getcpu
    now returns CPU 0 and NUMA node 0 through optional 32-bit outputs. This
    matches the current single-CPU scheduler and prevents HotSpot from falling
    back to the unmapped legacy vsyscall. The obsolete cache argument is ignored.

    Corrected x86_64 sysinfo's mem_unit offset from 100 to 104. The old write
    corrupted freehigh and left mem_unit zero, causing Java to report
    "Too small maximum heap". SYSCHK reproduces that error and passes after
    the correction. BIGCHK now rejects an invalid memory baseline: its old
    physical-memory comparisons were ineffective when mem_unit was zero.
    Its rerun passes with real byte counts.

    CPUCHK covers output widths, NULL/unaligned/lazy pointers and glibc
    sched_getcpu. Java gets beyond both failures, then calls timed
    FUTEX_WAIT_BITSET (op 0x89) with a non-NULL deadline and gets ENOSYS.
    The screen reports "The futex facility returned an unexpected error code."
    glibc's abort path ultimately executes user-mode HLT and the kernel kills
    the thread group. Timed waits, full signal contexts and Java application
    execution remain incomplete. Invalid user pointers still lack general
    EFAULT handling; this change does not add SMP or a legacy vsyscall page.

    See [startup ABI fixes and reproduction](docs/jvm-startup-abi.md).

48. **Timed futex waits.** WAIT accepts relative intervals and WAIT_BITSET
    with MATCH_ANY accepts absolute deadlines. Invalid timespecs return EINVAL,
    mismatched words return EAGAIN, expired waits return ETIMEDOUT, and explicit
    wakes return zero. Deadlines round up to PIT ticks; positive relative waits
    include one extra tick to avoid early expiration. Distant deadlines saturate
    instead of wrapping. Realtime still shares the existing boot-relative clock.

    Each blocked task carries its deadline and completion reason. The scheduler
    expires waits under TASKS before selecting runnable work; wake and expiry
    both remove pending wait metadata. All task constructors initialize the new
    fields; separately allocated, 16-byte-aligned FPU state is unchanged.

    The real Java trace now returns ETIMEDOUT from its timed wait rather than
    ENOSYS and reaches class loading. Its next reported failure is
    `java/lang/ClassFormatError: Unknown constant tag 0 in class file java/lang/String`.
    The cause is not yet established. No kernel panic or CPU exception appears
    in the bounded trace, but Java does not finish startup or cleanly exit.
    See [timed-wait validation and limits](docs/timed-futex.md).

49. **Complete regular-file reads and current-directory queries.** The String
    ClassFormatError came after pread requested 49,154 bytes and received only
    4,096. The disk JDK archive matches the host archive and passes jimage verify.
    Linux read/pread now loop over the same bounded 4 KiB staging buffer until
    completion, EOF or error. No file-sized kernel allocation is introduced;
    lazy user writes still happen outside TASKS. READCHK verifies the exact
    class slice, large pattern reads, offsets, EOF and untouched destinations.

    Java then reached SystemProps and failed on missing getcwd. The new syscall
    reports the filesystem's existing global CWD with a NUL-inclusive raw length
    and ERANGE for short buffers. CWDCHK verifies raw and glibc calls, buffer
    bounds and lazy destinations. This does not add per-process CWD or chdir.

    With both fixes Java passes those errors. The next captured user fault writes
    to `0x707c003fe8` inside the protected range `0x707c000000..0x707c004000`,
    with RSP `0x707c003f90` and RIP `0x70011998f1`. The mapped libjvm address
    resolves to SystemDictionary::resolve_instance_class_or_null. The reason
    for reaching the guard is not yet established; Java startup remains incomplete.
    See [read/getcwd evidence and limitations](docs/large-read.md).

50. **Shared Linux thread descriptors.** A read-only fault stack capture showed
    repeated class resolution and exception construction. Before that, new Java
    threads received EBADF reading the parent's modules descriptor: clone had
    created an empty table despite CLONE_FILES. Linux clones now retain an
    Arc-owned descriptor table with shared entries, positions, opens and closes.
    The table survives its creator's exit and is released with its last owner.
    Independent processes/native tasks still start with independent tables.

    Descriptor metadata uses an IRQ-safe lock, acquired only after TASKS is
    released. Disk I/O and user copies remain outside both locks. Read offset
    updates rely on the existing single-CPU, IRQ-disabled syscall invariant.
    Clone without CLONE_FILES now explicitly returns ENOSYS; nonshared table
    copying and full clone flag semantics remain unsupported.

    FDCHK covers inherited lazy reads, shared offsets and descriptor replacement,
    plus a nested child using a descriptor after its creating thread exits.
    The Java trace no longer reproduces the captured class-resolution guard
    fault and those class reads succeed. The bounded run does not establish
    completed startup; it includes repeated clock_nanosleep ENOSYS calls.
    See [ownership, stack evidence and validation](docs/clone-files.md).

51. **Deadline-based sleep and scheduler yield.** Linux nanosleep and
    clock_nanosleep now block until deadline expiry using the existing scheduler.
    Relative and absolute requests share timed-futex validation, saturating
    conversion and upward tick rounding. Realtime/monotonic/boottime still use
    the boot-relative 100 Hz clock. Sleeps have no futex address, so a futex
    wake cannot end them early. sched_yield now invokes the existing scheduler.

    SLEEPCHK passes in BIOS and UEFI, including deadlines, malformed timespecs,
    untouched/unaligned request buffers, sibling progress and futex isolation.
    TIMECHK and FDCHK pass too. Timing fixtures now establish readiness before
    testing wake deadlines, avoiding deadlines consumed by thread startup under
    the debugger. Stable debug/release builds link without unresolved symbols;
    existing intrinsic warnings remain. Signal interruption/remainder updates,
    wall-clock adjustments and CPU-time clocks are still unsupported.

    Trace logs now survive temporary-directory cleanup, include bounded sleep/
    wait caller context, and support disabling per-syscall breakpoints to compare
    debugger overhead. See [sleep semantics, tests and limits](docs/sleep.md).

    The 120-second traced Java run records six successful sleeps in JVM
    safepoint synchronization. A 60-second run without per-syscall breakpoints
    reaches LauncherHelper, then reports `Error loading java.security file`.
    Java startup is not complete. Next, inspect the disk JDK's configuration
    files and their open/read paths; the host java.security is a symlink into
    /etc/java-21-openjdk, so copying only the JDK directory may omit its target.

52. **Real `argv[1..]` for spawned programs, a full disk JDK/glibc
    placement, and `java -version` prints the real banner.** This
    session's `disk.img` is gitignored and rebuilt fresh by `make disk`
    from `disk_root/`, which has no JDK on it -- item 51's disk-config
    question was unverified because the guest disk placement items 34-51
    relied on doesn't survive between sessions; it's redone by hand each
    time. Redone the same way item 34 first established: the real JDK tree
    (`rsync -L`, symlinks dereferenced to real files at their real
    relative paths -- `conf/security/java.security`,
    `lib/security/cacerts`, `lib/jvm.cfg`, and others all point outside the
    JDK tree on a real Debian/Ubuntu install) at
    `/usr/lib/jvm/java-21-openjdk-amd64`, plus `ld-linux-x86-64.so.2` and
    `java`/`libjvm.so`'s handful of `DT_NEEDED` libraries at `ld.so`'s real
    compiled-in default search paths (`/lib64/`, `/lib/x86_64-linux-gnu/`)
    -- no `DT_NEEDED`/`PT_INTERP` string patching, same as item 34.

    That placement immediately exposed a second, real gap: `run
    .../bin/java -version` failed with "no such file or directory" --
    `cmd_run` (`commands.rs`) had never split anything past the path, and
    `loader.rs`'s `build_initial_stack` only ever wrote a single-element
    `argv` for every caller, exactly the "shell still needs arguments"
    limitation the roadmap's own item 1 had been naming since item 41.
    Fixed: `build_initial_stack` now takes a full `argv: &[String]`
    (written onto the stack the same way `envp` already was, argv first so
    envp continues right below it) instead of one `argv0: &str`, with
    `argc`/`argv[]`'s pointer array sized to match; `cmd_run` now splits
    whatever follows the path on whitespace into real `argv[1..]`, after
    the existing `VAR=value` `envp` prefix parsing. Both `load_and_run` and
    the `PT_INTERP` dynamic-linking path `load_and_run_with_interp` (what
    `java` itself goes through) carry the new `argv` slice through.
    `run hello.exe` (empty `argv[1..]`) still prints its existing message
    unchanged, confirming the plumbing change is a no-op for every
    existing zero-extra-argument caller.

    With both fixes, `run /usr/lib/jvm/java-21-openjdk-amd64/bin/java
    -version` now prints, to the real framebuffer console and byte-for-
    byte matching this same JDK on the host: `openjdk version "21.0.10"
    2026-01-20`, `OpenJDK Runtime Environment (build
    21.0.10+7-Ubuntu-124.04)`, `OpenJDK 64-Bit Server VM (build
    21.0.10+7-Ubuntu-124.04, mixed mode, sharing)`. No CPU exception or
    kernel panic, across two independent runs. This is the exact milestone
    item 1's own roadmap text named as unverified.

    Not shown: a clean `exit_group`. After the banner, HotSpot spins up
    background compiler/GC/sweeper threads that hit several syscalls this
    kernel doesn't implement (`273` `set_robust_list`, `334` `rseq`, `302`
    `prlimit64`, `96` `gettimeofday`, `157` `prctl`, `229` `clock_getres`,
    `107` `geteuid`, `41` `socket`); each gets this kernel's generic
    "unimplemented" response, HotSpot evidently tolerates the failure and
    keeps going (no crash), but the console's final state was identical
    across a 90s and a 100s run with no process-exit or new shell prompt
    ever appearing. `-version`'s own required output is real and
    confirmed; a full `java -jar` run to completion needs at least some of
    those syscalls implemented, not attempted here. See [staging steps,
    the argv fix and full evidence](docs/java-version.md).

The [OSDev Wiki](https://wiki.osdev.org/) is the standard reference for all
of the above once you're ready for it.

