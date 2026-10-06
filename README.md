# KonjacOS

KonjacOS is a hobby operating system for x86_64 PCs, written from scratch
in Rust. It boots on real BIOS and UEFI firmware, has its own kernel,
filesystem and shell, and can run unmodified Linux programs, DOOM, and
the first parts of a real Java runtime.

The long-term goal is to run Minecraft: Java Edition.

## Features

- Boots through the [Limine](https://limine-bootloader.org/) bootloader
  on BIOS and UEFI, straight into a desktop.
- A "liquid glass" desktop drawn entirely in software: squircle windows,
  a floating taskbar and menus made of glass that blurs, refracts and
  tints whatever is behind it.
- Preemptive multitasking, with each user program in its own address space.
- Virtual memory with demand paging, `mmap`, memory protection and
  no-execute pages.
- KonjacFS, its own copy-on-write filesystem ([design](docs/kfs-design.md)):
  every block is checksummed, and every change lands all at once or not at
  all, so a crash or power cut can't leave the disk half written. The
  FAT16 disk from earlier versions still works, at `/fat`.
- Runs ELF64 (static and dynamic), PE32+ `.exe` and flat binary programs.
- A Linux compatibility layer that runs unmodified glibc and musl
  programs, including threads and signals.
- DOOM, playable in a desktop window.
- OpenJDK 21: `java -version`, and simple Java programs run from a
  folder or a `.jar` file.

## Download and run

Get the latest `.iso` and `-kfs.zip` from the
[Releases](https://github.com/Jayd567/KonjacOS/releases) page, unzip the
disk image, then boot both in [QEMU](https://www.qemu.org/):

```sh
qemu-system-x86_64 -m 256M -boot order=d \
  -cdrom konjacos-v0.3.0.iso \
  -drive file=konjacos-v0.3.0-kfs.img,format=raw,if=virtio
```

The KonjacFS disk becomes `/`. To keep using a FAT16 disk from an
earlier version, add it as a second `-drive`: it appears at `/fat`, and
your settings and pins are copied over the first time. (A FAT16 disk on
its own, or attached with `if=ide`, is `/` as before.)

The ISO boots on its own, but without a disk image there are no files
to browse and no DOOM. For a smoother desktop, add `-accel whpx` on
Windows, `-accel kvm` on Linux or `-accel hvf` on macOS.

KonjacOS boots to the desktop with the Terminal open. Start lists every
app, and typing in it searches apps, settings and the files on the disk;
the taskbar shows the apps that are running plus any you pin
(right-click an app, then "Pin to Taskbar"). The "K" in the top-left
corner has About, the system monitor, Restart and Shut Down. Typing goes
to whichever window is in front: the shell in the Terminal, Notepad,
Files, or DOOM.

Files opens, creates, renames, copies, moves and deletes files and
folders, and Notepad edits text files; double-clicking a `.txt` file
opens it there.

The desktop starts empty: right-click an app, or a file or folder in
Files, and choose "Create Shortcut" to put it there. Pins, shortcuts
and Settings are saved to `DESKTOP.CFG` on the disk. Right-click almost
anything for a menu, resize windows from any edge or corner, and drag
them to a screen edge to snap them.

Keyboard shortcuts: Alt+Tab, Alt+F4, Super (Start), Super+arrows (snap),
Super+D (show desktop), Super+E (Files), Super+I (Settings),
Ctrl+Alt+T (Terminal). In QEMU, click into the window first so it has
grabbed the keyboard, or the host OS may take Alt+Tab and Super itself.

Things to try in the Terminal:

```
ls                 list files on the disk
ls /fat            list the FAT16 disk, if one is attached
cat README.TXT     print a file
run hello.exe      run a Windows-format program
doom               play DOOM (opens in its own window)
ps                 list running tasks
```

## Shell commands

| Command | Description |
| --- | --- |
| `help` | List all commands |
| `ls`, `cd`, `pwd` | Browse the filesystem |
| `cat <file>` | Print a file |
| `write <file> <text>` | Create or overwrite a file |
| `rm <file>` | Delete a file (needs the admin password) |
| `run <file> [args]` | Run a program |
| `ps`, `kill <id>` | List or stop running tasks |
| `doom` | Play DOOM |
| `meminfo`, `uptime` | Show memory use and uptime |
| `diskbench` | Time reads, writes and deletes on each disk |
| `verify [folder]` | Read every file and report any that are damaged |
| `kfstest [steps] [seed] [keep] [fill]` | Random writes, renames and deletes on KonjacFS, each checked; `fill` also fills the disk |
| `reboot`, `halt` | Restart or stop the machine (needs the admin password) |

The first time you run a command that needs the admin password, you
choose one. Only its SHA-256 hash is stored on the disk.

## Building from source

You need a Linux machine with:

- Rust (stable)
- `gcc` or `clang`
- Python 3 (for `tools/kfs.py`, which builds the KonjacFS disk)
- `xorriso`, `mtools`, `dosfstools`
- `qemu-system-x86_64` to run it

On Debian or Ubuntu:

```sh
sudo apt install build-essential xorriso mtools dosfstools qemu-system-x86
```

Then:

```sh
make run          # build and boot in QEMU (BIOS)
make run-uefi     # same, using UEFI firmware (needs the ovmf package)
make release      # build the downloadable images into dist/
```

Add `MODE=release` to any of these for an optimized build.

`make` builds the disks from `disk_root/` the first time and leaves them
alone after that. To add a file to the KonjacFS disk:

```sh
python3 tools/kfs.py put kfs.img myfile.txt /     # also: ls, cat, get, mkdir, rm, check
```

Two tests boot KonjacOS in QEMU over and over to check KonjacFS:

```sh
tools/crash_test.py 200     # kills QEMU mid-write; the disk must stay consistent
tools/bitrot_test.py 100    # damages a random block; it must be caught, never returned
```

## Roadmap

1. **Java.** Run larger Java programs and fill in the remaining Linux
   system calls they need.
2. **Networking.** A loopback TCP/IP stack, since even singleplayer
   Minecraft talks to a local server over a socket.
3. **Graphics.** OpenGL support for LWJGL, most likely through a software
   renderer.
4. **Minecraft.**

See [CHANGELOG.md](CHANGELOG.md) for what changed in each release.

## Project layout

| Path | Contents |
| --- | --- |
| `kernel/src/` | The kernel, in Rust |
| `kernel/src/ui/` | The desktop ([design notes](docs/desktop-ui-design.md)) |
| `kernel/assets/` | Wallpaper, logo, icons and fonts baked into the kernel |
| `kernel/csrc/` | C code built into the kernel, including the DOOM port |
| `disk_root/` | Files copied onto the disk images |
| `userprogs/` | Small test programs |
| `tools/` | Debugging scripts, the asset generator, `kfs.py` (KonjacFS images), the KonjacFS crash and bit-rot tests, and a QEMU screenshot harness |
| `boot/`, `limine/` | Bootloader configuration and files |
| `docs/` | Design notes and development history |

## License

KonjacOS is released under the [GNU AGPL v3](LICENSE). The DOOM port is
based on [doomgeneric](https://github.com/ozkl/doomgeneric) (GPL v2), and
`DOOM1.WAD` is id Software's freely distributable shareware release.
