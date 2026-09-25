; thread_elf.asm -- exercises SYS_CLONE (syscall.rs): the parent thread
; carves out an 8 KiB stack region for a second thread by growing its own
; heap with brk, then clones a sibling that shares its address space
; instead of getting a fresh private one. Both threads print their own
; message through the same int 0x80 -> syscall_handler path, proving two
; tasks really are running concurrently against one shared set of page
; tables -- not just two processes that happen to look similar.
bits 64

section .text
global _start
_start:
    ; brk(0) -> current heap top into r12
    xor rdi, rdi
    mov rax, 5          ; SYS_BRK
    int 0x80
    mov r12, rax

    ; brk(top + 8192) -> map 8 KiB for the child thread's stack
    lea rdi, [r12 + 8192]
    mov rax, 5          ; SYS_BRK
    int 0x80

    ; clone(thread_entry, stack_top = r12 + 8192) -- stack grows down from
    ; the top of the freshly-mapped region, same convention every other
    ; stack in this kernel uses.
    lea rdi, [rel thread_entry]
    lea rsi, [r12 + 8192]
    mov rax, 6          ; SYS_CLONE
    int 0x80
    mov r13, rax        ; r13 <- child task id (or -1 on failure)

    lea rdi, [rel parent_msg]
    mov rsi, [rel parent_msg_len]
    mov rax, 1          ; SYS_WRITE
    int 0x80

    ; Busy-spin (no syscalls -- just burning CPU cycles) to give the
    ; preemptive, timer-driven scheduler plenty of 100 Hz ticks to actually
    ; run the child thread before the parent exits. Preemption happens on
    ; every timer interrupt regardless of what ring-3 code is doing, so
    ; this doesn't need to cooperate with the child at all -- there's no
    ; join/wait syscall yet, so a generous spin stands in for real
    ; synchronization.
    mov rcx, 50000000
.spin:
    dec rcx
    jnz .spin

    mov rax, 0          ; SYS_EXIT
    int 0x80
hang:
    jmp hang

thread_entry:
    lea rdi, [rel child_msg]
    mov rsi, [rel child_msg_len]
    mov rax, 1          ; SYS_WRITE
    int 0x80
    mov rax, 0          ; SYS_EXIT
    int 0x80
child_hang:
    jmp child_hang

parent_msg:
    db "[parent] hello from the main thread", 10
parent_msg_end:
parent_msg_len:
    dq parent_msg_end - parent_msg

child_msg:
    db "[child] hello from a cloned thread, same address space", 10
child_msg_end:
child_msg_len:
    dq child_msg_end - child_msg
