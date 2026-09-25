; mmap_elf.asm -- exercises SYS_MMAP (syscall.rs) and, through it,
; paging::handle_page_fault: reserves a page of anonymous memory that is
; deliberately never touched by SYS_BRK, then writes into it directly.
; That first write has no physical frame behind it yet -- the reservation
; alone doesn't map anything -- so it faults, gets serviced by the new #PF
; handler (which allocates, zeroes, and maps a real frame, then retries
; the very same instruction), and only then succeeds. Reading the message
; back out afterward proves the page that fault produced is genuinely
; both writable and readable, not just "didn't crash."
bits 64

section .text
global _start
_start:
    mov rdi, 4096
    mov rax, 7          ; SYS_MMAP
    int 0x80
    mov r12, rax        ; r12 <- base of a freshly *reserved*, not-yet-backed page

    ; Copy msg into the reserved region one byte at a time. The very first
    ; store below is what actually triggers the page fault -- everything
    ; after that lands on a now-present page like any ordinary write.
    lea rsi, [rel msg]
    xor rdx, rdx
.copy:
    mov al, [rsi + rdx]
    mov [r12 + rdx], al
    inc rdx
    cmp rdx, msg_len
    jl .copy

    ; Read it back out through SYS_WRITE -- proves the fault-mapped page
    ; is really there, not just silently tolerated.
    mov rdi, r12
    mov rsi, msg_len
    mov rax, 1          ; SYS_WRITE
    int 0x80

    mov rax, 0          ; SYS_EXIT
    int 0x80
hang:
    jmp hang

msg:
    db "mmap demand-paged memory works: never touched by brk, only by a page fault.", 10
msg_end:
msg_len equ msg_end - msg
