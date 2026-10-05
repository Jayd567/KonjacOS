# Changelog

Each version here is published on the
[Releases](https://github.com/Jayd567/KonjacOS/releases) page with
ready-to-boot images.

## Unreleased

### New

- A desktop, which KonjacOS now boots straight into after the "K" logo:
  - Liquid glass everywhere: squircle shapes from a signed distance field,
    blur that grows with how high a panel floats, Snell's-law refraction
    with colour fringing at the rim, a tint taken from the backdrop, a
    dither layer and a top-edge light catch.
  - A floating taskbar with Start, pinned apps and a live tray (CPU,
    memory, clock), and a transparent top bar with glass dropdown menus.
  - Windows that drag, maximize (above the taskbar), minimize and close,
    with spring animations and hit-testing that follows their rounded
    corners.
  - Apps: Terminal (the shell), Files, Monitor, DOOM and About.
- The `doom` command and the taskbar both open DOOM in a desktop window.

### Changed

- The `gui` command is gone: the desktop is always running.
- `memcpy`/`memset` use `rep movsb`/`rep stosb` instead of byte loops.

### Fixed

- A task that yielded could resume with interrupts disabled and never be
  preempted again, freezing the machine.
- The kernel heap could deadlock when a task was preempted mid-allocation
  and something then allocated or freed with interrupts disabled.
- If every task was blocked, the scheduler kept running a blocked task.
  An idle task now takes the CPU instead, and the two always-busy demo
  threads that used to mask this are gone.
- A stray byte from the PS/2 mouse could misalign every packet after it.

## v0.1.0

The first public release of KonjacOS.

### What works

- Boots on BIOS and UEFI through the Limine bootloader.
- A text shell with line editing and a
  `help` command that lists everything it can do.
- Preemptive multitasking with kernel threads and isolated user programs.
- Memory management: physical frame allocator, paging, kernel heap,
  demand paging, `mmap`, `mprotect` and no-execute pages.
- FAT16 filesystem on an ATA disk, with folders, long filenames, and
  file create, write and delete.
- Runs flat binaries, ELF64 (static and dynamically linked) and PE32+
  `.exe` programs.
- A Linux compatibility layer large enough to run unmodified glibc and
  musl programs, including threads, futexes and signals.
- DOOM (shareware), playable in a window.
- A basic window manager with draggable windows.
- OpenJDK 21: `java -version`, `java -cp` and `java -jar` run simple
  programs to completion, including programs that call `System.exit`.

### Known issues

- Larger Java programs are untested and will likely need more Linux
  system calls.
- No networking yet.
