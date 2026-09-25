# Shared file descriptors for Linux thread clones

## Diagnosis

The Java stack-guard fault was downstream of failed class reads in a new
thread. The previous trace records that thread's pread calls on descriptor 0
returning EBADF, including offsets 0x7985a3, 0x90b14d and 0x90af48. The parent
had successfully used that same descriptor for the JDK modules archive.

The read-only frame-pointer walk captures 128 frames: 32 repetitions of
SystemDictionary::resolve_or_fail, Exceptions::new_exception,
Exceptions::_throw_msg and SystemDictionary::resolve_instance_class_or_null.
See [the captured frames and resolved symbols](java-stack-before-files.json).
The old spawn_clone_raw initialized an empty descriptor table despite glibc's
CLONE_FILES request. The focused FDCHK reproduces failure on its first child
read, before involving the JVM or a stack guard.

## Ownership and locking

Each independent process/native task starts with an Arc-owned, IRQ-safe locked
descriptor table. Linux clone/clone3's supported thread path requires
CLONE_FILES and retains the parent's Arc. Descriptor entries, positions, opens
and closes are then shared. The table is freed when its final owner is reaped,
not when the creating thread exits. The table remains bounded by MAX_OPEN_FILES.

The current table is retained under TASKS; that lock is released before the
descriptor lock is taken. Descriptor closures only touch metadata. Disk reads
snapshot backing and offset, release the table lock, perform I/O into resident
kernel memory, then update the entry under the table lock. No user copy, I/O or
scheduling is permitted while holding the descriptor lock. The existing
single-CPU, IRQ-disabled syscall invariant keeps the descriptor/offset stable
between those short lock sections. This is not an SMP design.

FPU state stays separately allocated with its existing 16-byte alignment and
512-byte size assertions. No assembly frame offsets or calling conventions
change. Arc uses alloc in the existing freestanding kernel; no hosted runtime
or unwinding is introduced.

## Regression

FDCHK opens a file in the parent, reads through it in a glibc-created child into
untouched pages, checks the parent's shared sequential position, closes/reopens
from another child, and verifies the replacement from the parent. It also checks
that a nested child can use a descriptor after its creating thread has exited.
The old kernel failed the first inherited-descriptor check; the new one passes.

Final FDCHK passes in BIOS and UEFI. READCHK, VMCHK and TIMECHK also pass,
covering large file reads, shared-thread mappings, permissions, rollback and
timed waits. None of those five regression boots has a CPU exception or kernel
panic. Stable debug/release builds succeed with six existing intrinsic warnings
and no unresolved symbols; the C fixture passes `-Wall -Wextra`. Runtime tests
use the debug kernel. Review found no blocking issue in reference lifetimes,
lock order or the unchanged FPU alignment.
See [saved regression results and kernel hash](clone-files-validation.json).

Reproduce from WSL, one QEMU at a time:

```sh
make iso clone-files-fixture
python3 tools/trace_jvm.py files-final "run fdchk.elf" 40
KONJAC_UEFI=1 python3 tools/trace_jvm.py files-uefi "run fdchk.elf" 40
python3 tools/trace_jvm.py files-read "run readchk.elf" 35
python3 tools/trace_jvm.py files-vm "run vmchk.elf" 40
python3 tools/trace_jvm.py files-timed "run timechk.elf" 40
KONJAC_GDB_EXTRA=tools/trace_java_stack_gdb.py python3 tools/trace_jvm.py java-files "run /usr/lib/jvm/java-21-openjdk-amd64/bin/java" 120
make MODE=release kernel
```

## Java follow-up

Historical checkpoint: [sleep and yield support](sleep.md) now implements the
missing syscalls identified below and adds durable diagnostic traces.

Both bounded 120-second traces (default and JAVA_TOOL_OPTIONS=-Xint) have zero
pread EBADF returns, no captured guard faults and no CPU exception/kernel panic.
The default run reports 26 unsupported clock_nanosleep (syscall 230) calls.
The interpreter run acknowledges -Xint and continues clock/futex activity without
printing completed startup output; it records no clock_nanosleep calls within
that window. Thus unsupported sleep is an observed gap, not yet a proven sole
cause of incomplete startup. Next inspect the main Java thread's wait state and
the sleep callers, implement/test missing sleep semantics where required, and
rerun. Neither trace proves java -version, Java application execution or clean exit.

## Remaining limits

clone without CLONE_FILES now returns ENOSYS rather than creating an empty or
incorrectly shared table. Proper nonshared descriptor-table copies would still
need shared open-file descriptions. The existing clone path is thread-only and
does not fully enforce or implement all other clone flags. Signal actions and
brk remain per-task, CWD remains global, and native spawn_thread still starts
with an independent empty table. General user-pointer validation, SMP and
blocking/asynchronous I/O remain outside this change.
