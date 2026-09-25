; hello_elf.asm -- assembled as a real static ELF64 executable (see
; build.sh: `nasm -f elf64` + `ld -static -no-pie`). Proves KonjacOS's
; loader.rs can parse a genuine ELF64 header and PT_LOAD program headers,
; not just a format it invented itself.
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
    jmp hang            ; unreachable; safety net only

msg:
    db "Hello from a real ELF64 executable! KonjacOS's loader parsed genuine PT_LOAD program headers to run this.", 10
msg_end:
msglen:
    dq msg_end - msg
