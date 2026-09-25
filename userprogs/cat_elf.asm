; cat_elf.asm -- exercises every syscall added on top of the original
; write+exit pair (see syscall.rs): opens a real file off the FAT16 disk,
; grows its own heap with brk to get a real destination buffer instead of
; a fixed-size one baked into the binary, reads the file into that fresh
; memory, writes it back out (proving the mapped heap page is genuinely
; both readable and writable), then closes the fd before exiting. `int
; 0x80` only ever clobbers rax (see syscall.rs's stub doc comment -- every
; other register is saved/restored around the call), so plain general
; registers double as this program's own scratch storage across syscalls
; with no extra bookkeeping needed.
bits 64

section .text
global _start
_start:
    ; open("HELLO.TXT") -> fd in r12
    lea rdi, [rel path]
    mov rsi, [rel pathlen]
    mov rax, 2          ; SYS_OPEN
    int 0x80
    mov r12, rax        ; r12 <- fd (or -1 on failure; not checked here,
                         ; this is a happy-path demo, not hardened code)

    ; brk(0) -> query the current heap top into r13
    xor rdi, rdi
    mov rax, 5          ; SYS_BRK
    int 0x80
    mov r13, rax        ; r13 <- current heap top == the fresh buffer's address

    ; brk(top + 4096) -> actually map one page there
    lea rdi, [r13 + 4096]
    mov rax, 5          ; SYS_BRK
    int 0x80            ; rax <- new heap top (ignored; r13 already has
                         ; the buffer's start address, which is all that's
                         ; needed)

    ; read(fd, r13, 4096) -> bytes actually read, kept in r14
    mov rdi, r12
    mov rsi, r13
    mov rdx, 4096
    mov rax, 3          ; SYS_READ
    int 0x80
    mov r14, rax        ; r14 <- bytes read

    ; write(r13, r14) -- print exactly what was just read back out
    mov rdi, r13
    mov rsi, r14
    mov rax, 1          ; SYS_WRITE
    int 0x80

    ; close(fd)
    mov rdi, r12
    mov rax, 4          ; SYS_CLOSE
    int 0x80

    mov rax, 0          ; SYS_EXIT
    int 0x80
hang:
    jmp hang

path:
    db "HELLO.TXT"
path_end:
pathlen:
    dq path_end - path
