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
- A FAT16 filesystem with folders and long filenames, readable and
  writable by any other OS.
- Runs ELF64 (static and dynamic), PE32+ `.exe` and flat binary programs.
- A Linux compatibility layer that runs unmodified glibc and musl
  programs, including threads and signals.
- DOOM, playable in a desktop window.
- OpenJDK 21: `java -version`, and simple Java programs run from a
  folder or a `.jar` file.

## Download and run

Get the latest `.iso` and `-disk.zip` from the
[Releases](https://github.com/Jayd567/KonjacOS/releases) page, unzip the
disk image, then boot both in [QEMU](https://www.qemu.org/):

```sh
qemu-system-x86_64 -m 256M -boot order=d \
  -cdrom konjacos-v0.1.0.iso \
  -drive file=konjacos-v0.1.0-disk.img,format=raw,if=ide
```

The ISO boots on its own, but without the disk image there are no files
to browse and no DOOM. For a smoother desktop, add `-accel whpx` on
Windows, `-accel kvm` on Linux or `-accel hvf` on macOS.

KonjacOS boots to the desktop with the Terminal open. The taskbar has
Start, Terminal, Files, Monitor, DOOM and About; the "K" in the top-left
corner has About, the system monitor, Restart and Shut Down. Typing goes
to the shell (or to DOOM, while its window is in front).

The desktop has icons for every app (including Sketch, a small drawing
app) and for the files on the disk. Drag icons around, drop an app on
the taskbar to pin it, and right-click almost anything for a menu.
Windows resize from any edge or corner.

Things to try in the Terminal:

```
ls                 list files on the disk
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
| `reboot`, `halt` | Restart or stop the machine (needs the admin password) |

The first time you run a command that needs the admin password, you
choose one. Only its SHA-256 hash is stored on the disk.

## Building from source

You need a Linux machine with:

- Rust (stable)
- `gcc` or `clang`
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
| `disk_root/` | Files copied onto the disk image |
| `userprogs/` | Small test programs |
| `tools/` | Debugging scripts, the asset generator and a QEMU screenshot harness |
| `boot/`, `limine/` | Bootloader configuration and files |
| `docs/` | Design notes and development history |

## License

KonjacOS is released under the [GNU AGPL v3](LICENSE). The DOOM port is
based on [doomgeneric](https://github.com/ozkl/doomgeneric) (GPL v2), and
`DOOM1.WAD` is id Software's freely distributable shareware release.
