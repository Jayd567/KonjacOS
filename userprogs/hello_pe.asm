; hello_pe.asm -- assembled as a real PE32+ (.exe) executable (see
; build.sh: `nasm -f win64` + `x86_64-w64-mingw32-ld`). Proves KonjacOS's
; loader.rs can parse a genuine PE header and section table, not just an
; ELF-derived format. Identical logic to hello_elf.asm -- same two
; KonjacOS syscalls, same message shape -- deliberately, to make the point
; that this is one OS's ABI wearing two different container formats, not
; two different programs.
bits 64

section .text
global _start
_start:
    lea rdi, [rel msg]
    mov rsi, [rel msglen]
    mov rax, 1          ; SYS_WRITE
    int 0x80

    mov rax, 0          ; SYS_EXIT
    int 0x80
hang:
    jmp hang

msg:
    db "Hello from a real PE32+ (.exe) executable! KonjacOS's loader parsed a genuine PE section table to run this.", 10
msg_end:
msglen:
    dq msg_end - msg
