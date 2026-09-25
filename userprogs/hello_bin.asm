; hello_bin.asm -- assembled as a raw flat binary (see build.sh:
; `nasm -f bin`), no header of any kind: the file itself *is* the machine
; code, loaded verbatim at loader.rs's FLAT_BASE with the first byte as the
; entry point.
bits 64

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
    db "Hello from a raw flat binary! No header at all -- KonjacOS's loader just ran the file's bytes directly.", 10
msg_end:
msglen:
    dq msg_end - msg
