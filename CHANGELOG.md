# Changelog

Each version here is published on the
[Releases](https://github.com/Jayd567/KonjacOS/releases) page with
ready-to-boot images.

## Unreleased

### New

- ks, KonjacShell ([design](docs/ks-design.md)), is the Terminal's shell
  now. Its commands pass values to each other instead of text:
  - `ls` gives a table of name, type, size and modified date, so
    `ls | filter size > 1MB | sort-by modified` works on real sizes and
    dates, and the result shows as an aligned table.
  - Values have types: numbers, text, sizes (`50MB` is 1000-based,
    `50MiB` 1024-based), durations, dates, lists, records, tables and
    closures. Arithmetic keeps units (`1MB + 500KB` is `1.5 MB`), and
    mixing them up is an error with a hint: `size > 5` says "5 has no
    unit; did you mean 5MB?".
  - A failing step stops the pipeline. The error names the step and the
    item, and points at the place in the line. Unknown commands and flags
    are caught before anything runs, with a suggestion ("did you mean
    `ls`?").
  - `delete` looks everything up before deleting anything, and asks
    first when files are piped in: "delete 3 files (209 B)?". `--dry-run`
    shows what it would delete, and `move` and `copy` refuse to start if
    anything is missing or in the way.
  - `let`, `mut`, `def` (with typed parameters and flags), `if`, `for`,
    `while`, closures (`{|f| $f.size > 1MB}`), string interpolation
    (`"hi $name, (1 + 2)"`) and `.ks` scripts (`source`).
  - About 50 new commands: `filter`, `sort-by`, `select`, `get`, `each`,
    `group-by`, `open` (`.json` files become values), `save`, `str ...`,
    `math ...`, `from json`/`to json`, `disks`, `mem`, `stat`, and more.
    `help` is a table of them all; `help <command>` explains one.
  - The original commands (`run`, `doom`, `diskbench`, `write`, ...)
    still work as before.
  - Ctrl+C stops a running command, or cancels the line being typed.
- The language is its own crate (`ks/`), and `cargo test` there runs 15
  tests on the host in about a second, including one that feeds it
  thousands of random lines to check it never crashes the kernel.
- The shell's stack is 1 MiB (it was Limine's default 64 KiB), since ks
  evaluates nested expressions and calls recursively.
- `tools/qemu_shot.sh` can type symbols (`| > " * { }` and more).

### Changed

- KonjacFS is now faster than FAT16 at nearly everything (`diskbench`,
  median of 5 runs, first-time reads):

  | | KonjacFS | FAT16 |
  |---|---:|---:|
  | Read DOOM1.WAD (4 MB) | 9.0 ms | 13.7 ms |
  | Read it in 4 KiB pieces | 31.1 ms | 160.1 ms |
  | 256 random 4 KiB reads | 21.0 ms | 137.1 ms |
  | Write 1 MiB | 6.5 ms | 37.1 ms |
  | List a folder x20 | 0.9 ms | 2.9 ms |

  Reading DOOM1.WAD took 30.6 ms before. Deleting is the exception
  (2.1 against 1.2 ms), because a KonjacFS delete is safely on the disk
  when it returns and a FAT16 one isn't.
- How:
  - `memcpy` and `memset` were the real bottleneck: `rep movsb` runs
    byte by byte under QEMU without acceleration, 36 MB/s. They now copy
    64 bytes at a time through SSE2 (1.1 GB/s) and fill with `rep stosq`.
    Every file read and every frame the desktop draws goes through them.
  - virtio-blk hands the disk the caller's own memory instead of copying
    through a 64 KiB buffer, moves up to 1 MiB per request, and keeps 4
    requests in flight, which QEMU works on in parallel.
  - KonjacFS reads neighbouring extents together and checks each one's
    checksum while the rest are still arriving; small reads go through
    an 8 MiB cache with 256 KiB of read-ahead; a file's extents and a
    commit's blocks are written together.
- `diskbench` times reading in 4 KiB pieces and random 4 KiB reads too,
  and shows the median of 5 runs, timed to a tenth of a millisecond.

## v0.3.0

### New

- KonjacFS, KonjacOS's own copy-on-write filesystem
  ([design](docs/kfs-design.md)), is now the main disk, `/`. `make`
  builds it as `kfs.img` (512 MiB); the FAT16 disk, if attached too, is
  at `/fat`, and settings, pins and the admin password are copied over
  from it the first time.
  - Every block is checksummed, so a damaged disk gives an error instead
    of wrong data. If the newest commit itself is damaged, KonjacOS
    mounts the one before it and says so.
  - Writing is copy-on-write: each change (a save, a new folder, a
    rename, a delete) is written to free space and then made live by a
    single superblock write, so a crash can't leave the disk half
    changed. Old blocks are reused only once the change has landed.
  - Tested by killing QEMU mid-write 200 times and damaging random
    blocks 100 times (`tools/crash_test.py`, `tools/bitrot_test.py`):
    the disk always checked out, and damage was always reported, never
    returned as data.
  - Names are case-sensitive. Writing 1 MiB takes 10 ms, against 40 ms
    on FAT16.
  - `tools/kfs.py` builds, reads, checks and changes images on the host
    (`put`, `mkdir`, `rm`, like `mtools` for FAT16).
- `verify` in the Terminal reads every file and reports any that are
  damaged.
- A virtio-blk disk driver: the disk moves data into memory itself (DMA),
  64 KiB per request, instead of the CPU copying every sector through an
  I/O port. `make run` attaches the disk this way; the ATA driver is
  still used when the disk is attached as IDE.
- `diskbench` in the Terminal times reading, writing and deleting files.
- `kfstest` in the Terminal runs random writes, overwrites, renames and
  deletes on KonjacFS and checks every file as it goes; `kfstest fill`
  also fills the disk and checks the space all comes back.
- Several disks at once; each filesystem finds its own.
- Files copies and moves between the two disks; `ls` takes a folder.
- The release has a `-kfs.zip` (the KonjacFS disk) alongside the
  FAT16 `-disk.zip`.

### Changed

- The kernel reaches files through one layer (`vfs.rs`) that owns the
  current directory and routes each path to FAT16 or KonjacFS.
- The FAT16 driver keeps the FAT in memory, reads and writes whole runs
  of clusters per request, and writes changed FAT sectors once per
  operation instead of once per cluster. With virtio, writing 1 MiB went
  from 11.2 s to 40 ms and reading DOOM1.WAD from 1.3 s to about 10 ms.
- `diskbench` times KonjacFS and FAT16 side by side.
- `statfs` and Settings' Storage page report the disk at `/` and its
  format.
- DOOM is handed its WAD's exact path, since KonjacFS names are
  case-sensitive and DOOM only looks for `doom1.wad` in lower case.

### Fixed

- The kernel heap now merges freed memory with free neighbours. Before,
  long runs of mixed small and large allocations broke free memory into
  pieces until a 300 KB allocation could fail with most of the heap
  free.
- Files checks for a name clash the way the disk does, so pasting
  `HELLO.EXE` next to `hello.exe` on KonjacFS no longer makes a
  " - Copy".

## v0.2.0

### New

- A desktop, which KonjacOS now boots straight into after the "K" logo:
  - Liquid glass everywhere: squircle shapes from a signed distance field,
    blur that grows with how high a panel floats, Snell's-law refraction
    with colour fringing at the rim, a tint taken from the backdrop, a
    dither layer and a top-edge light catch.
  - A floating taskbar with Start, the running apps and any you pin
    from their right-click menu, and a live tray (CPU, memory, clock);
    a transparent top bar with glass dropdown menus.
  - Windows that drag, resize from any edge or corner, maximize (above
    the taskbar), minimize and close, with spring animations and
    hit-testing that follows their rounded corners.
  - Desktop shortcuts: the desktop starts empty, and "Create Shortcut"
    in the right-click menu of any app (in Start or on the taskbar) or
    any file or folder (in Files) puts one there. Double-click to open,
    drag to rearrange, rubber-band to select several. Shortcuts and
    pinned apps are saved to `DESKTOP.CFG` on the disk.
  - Right-click menus on the desktop, icons, taskbar items, the Start
    button, title bars, and inside the Terminal, Files and Sketch.
  - Mouse pointers that change with what's under them: resize arrows,
    a text cursor, a pen, a link hand, "not allowed", a spinner while an
    app starts, and more. All 17 come from the same cursor pack as the
    arrow.
  - Window snapping: drag a window to the left or right edge for half
    the screen, or to the top to maximize, with a glass preview of where
    it will land.
  - Keyboard shortcuts: Alt+Tab (with a glass app switcher), Alt+F4,
    Super for Start, Super+arrows to snap, Super+D (show desktop),
    Super+E (Files), Super+I (Settings), Ctrl+Alt+T (Terminal), and
    arrows/Enter/Esc in menus and Start.
  - Tooltips on the tray's memory, CPU and clock.
  - Search in Start: start typing to find apps, settings, and files and
    folders anywhere on the disk; arrows and Enter open a result.
  - Window animations: windows shrink into their taskbar button when
    minimized, grow back out of it when restored, and fade away when
    closed.
  - Scroll-wheel support, for Files and Notepad.
  - Apps: Terminal (the shell), Files, Notepad, Monitor, DOOM, Sketch (a
    small drawing app), Settings and About. Settings is split into
    sections (Personalization, Mouse, Date & time, Storage, System) with
    a search box: wallpaper, accent colour, glass strength, pointer and
    double-click speed, 12/24-hour clock, disk space and system info.
  - Notepad edits text files on the disk: selection with the mouse or
    Shift, cut/copy/paste, undo/redo, Save and Save As, line numbers. It
    asks before closing (or opening another file) with unsaved changes.
    Opening a text file anywhere on the desktop opens it here.
  - Files manages the disk: New Folder, New Text Document, Rename (in
    place), Delete (after asking), Cut, Copy and Paste, from the toolbar,
    the right-click menu or the keyboard (Delete, F2, Ctrl+C/X/V,
    Ctrl+Shift+N, Enter, Backspace). Desktop shortcuts follow a file that
    is renamed or moved, and disappear when it is deleted.
- The `doom` command and the taskbar both open DOOM in a desktop window.

### Changed

- The `gui` command is gone: the desktop is always running.
- `memcpy`/`memset` use `rep movsb`/`rep stosb` instead of byte loops.

### Fixed

- Writing a new file whose long name shared its first eight letters with
  another file's (`New Text Document.txt`, `New Text Document 2.txt`)
  overwrote that file. New long names now get unique `~1`-style short
  names, and deleting a file also removes its long-name entries.
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
