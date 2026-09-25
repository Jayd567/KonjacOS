#!/usr/bin/env bash
# Rebuilds the demo programs (see loader.rs/syscall.rs and README.md
# roadmap items 12-13) from their hand-written asm sources and drops the
# results straight into disk_root/, ready for `make disk`.
#
# Needs: nasm, GNU ld (both normally already present for building the
# kernel itself), and mingw-w64 for the PE build
# (`apt install mingw-w64` on Debian/Ubuntu).
set -euo pipefail
cd "$(dirname "$0")"

echo "== ELF64 (nasm -f elf64 + ld -static -no-pie) =="
nasm -f elf64 hello_elf.asm -o hello_elf.o
ld -static -nostdlib -no-pie -e _start -o hello.elf hello_elf.o

echo "== PE32+ (nasm -f win64 + x86_64-w64-mingw32-ld) =="
nasm -f win64 hello_pe.asm -o hello_pe.o
x86_64-w64-mingw32-ld -e _start -o hello.exe hello_pe.o

echo "== flat binary (nasm -f bin) =="
nasm -f bin hello_bin.asm -o hello.bin

echo "== cat.elf (open/read/brk/write/close demo, same toolchain as hello.elf) =="
nasm -f elf64 cat_elf.asm -o cat_elf.o
ld -static -nostdlib -no-pie -e _start -o cat.elf cat_elf.o

echo "== thread.elf (SYS_CLONE demo: a second thread sharing one address space) =="
nasm -f elf64 thread_elf.asm -o thread_elf.o
ld -static -nostdlib -no-pie -e _start -o thread.elf thread_elf.o

echo "== mmap.elf (SYS_MMAP demo: a page backed only by a page fault) =="
nasm -f elf64 mmap_elf.asm -o mmap_elf.o
ld -static -nostdlib -no-pie -e _start -o mmap.elf mmap_elf.o

echo "== pie.elf (ET_DYN/PIE demo: a real R_X86_64_RELATIVE self-relocation) =="
nasm -f elf64 pie_elf.asm -o pie_elf.o
ld -pie --no-dynamic-linker -e _start -o pie.elf pie_elf.o

cp hello.elf hello.exe hello.bin cat.elf thread.elf mmap.elf pie.elf ../disk_root/
rm -f hello_elf.o hello_pe.o cat_elf.o thread_elf.o mmap_elf.o pie_elf.o
echo "== done -- copied to disk_root/, run 'make disk' (after rm -f disk.img) to pick them up =="
