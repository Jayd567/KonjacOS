; pie_elf.asm -- exercises loader.rs's new ET_DYN/PIE support: `dq msg` in
; a writable data section is an *absolute* 64-bit pointer that the linker
; can't resolve to a compile-time constant in a position-independent
; executable (the final load address isn't known until KonjacOS actually
; picks one), so it becomes a real R_X86_64_RELATIVE dynamic relocation --
; confirmed against this exact binary with `readelf -r` before any kernel
; code was written to handle it (see the loader.rs commit). If the
; relocation is applied correctly, ptr_to_msg holds the *real*, biased
; address of msg once this program starts running, and reading through it
; naturally proves the fixup worked; there's no other way this program
; could print the right message.
;
; Built with `ld -pie --no-dynamic-linker` (not `-static -no-pie` like
; every other ELF demo here) -- see build.sh.
bits 64

section .text
global _start
_start:
    mov rax, [rel ptr_to_msg]   ; rax <- the *relocated* address of msg
    mov rdi, rax
    mov rsi, msg_len
    mov rax, 1                  ; SYS_WRITE
    int 0x80
    mov rax, 0                  ; SYS_EXIT
    int 0x80
hang:
    jmp hang

section .data
ptr_to_msg:
    dq msg                      ; absolute pointer -- needs R_X86_64_RELATIVE at load time

section .rodata
msg:
    db "hello from a self-relocating PIE (ET_DYN) executable!", 10
msg_end:
msg_len equ msg_end - msg
