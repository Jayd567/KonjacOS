# Changelog

Each version here is published on the
[Releases](https://github.com/Jayd567/KonjacOS/releases) page with
ready-to-boot images.

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
