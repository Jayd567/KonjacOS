# Changelog

Each version here is published on the
[Releases](https://github.com/Jayd567/KonjacOS/releases) page with
ready-to-boot images.

## Unreleased

### New

- A virtio-blk disk driver: the disk moves data into memory itself (DMA),
  64 KiB per request, instead of the CPU copying every sector through an
  I/O port. `make run` attaches the disk this way; the ATA driver is
  still used when the disk is attached as IDE.
- `diskbench` in the Terminal times reading, writing and deleting files.
- KonjacFS, KonjacOS's own copy-on-write filesystem
  ([design](docs/kfs-design.md)), on a second disk, `kfs.img`, built by
  `make kfs` and mounted at `/kfs`:
  - Every block is checksummed, so a damaged disk gives an error instead
    of wrong data.
  - Writing is copy-on-write: each change (a save, a new folder, a
    rename, a delete) is written to free space and then made live by a
    single superblock write, so a crash can't leave the disk half
    changed. Old blocks are reused only once the change has landed.
  - Files, Notepad and the Terminal work on it like on the FAT16 disk.
    Names are case-sensitive.
  - Writing 1 MiB takes 10 ms, against 40 ms on FAT16.
  - `tools/kfs.py` builds, reads and checks images on the host.
- `kfstest` in the Terminal runs random writes, overwrites, renames and
  deletes on KonjacFS and checks every file as it goes; `kfstest fill`
  also fills the disk and checks the space all comes back.
- Several disks at once; each filesystem finds its own.
- Files copies and moves between the two disks; `ls` takes a folder.

### Changed

- The kernel reaches files through one layer (`vfs.rs`) that owns the
  current directory and routes each path to FAT16 or KonjacFS.
- The FAT16 driver keeps the FAT in memory, reads and writes whole runs
  of clusters per request, and writes changed FAT sectors once per
  operation instead of once per cluster. With virtio, writing 1 MiB went
  from 11.2 s to 40 ms and reading DOOM1.WAD from 1.3 s to about 10 ms.
- `diskbench` times FAT16 and KonjacFS side by side.

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
