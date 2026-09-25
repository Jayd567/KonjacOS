# Timed futex waits

The previous Java trace called private WAIT_BITSET with a non-NULL absolute
deadline, received ENOSYS, and aborted in glibc. TIMECHK reproduced this before
the change with `FAIL relative timeout errno=38`.

## Implementation and invariants

`linux_syscall.rs` validates signed x86_64 timespec seconds/nanoseconds and
converts deadlines to PIT ticks using saturating arithmetic. WAIT uses relative
time, WAIT_BITSET uses absolute time. These semantics follow the Linux
[WAIT](https://man7.org/linux/man-pages/man2/FUTEX_WAIT.2const.html) and
[WAIT_BITSET](https://man7.org/linux/man-pages/man2/FUTEX_WAIT_BITSET.2const.html)
interfaces. Absolute deadlines round up. Positive relative intervals add one
tick beyond that rounding because the current tick may already be almost over.
The clock is 100 Hz; scheduling may delay return further. Zero intervals and
past deadlines expire immediately, after the futex-value mismatch check.

The scheduler scans blocked tasks for expired deadlines under TASKS. Wake and
timeout each transition only Blocked tasks, remove the address/deadline and
record the return reason. Thus a later scheduler pass cannot change a wake into
a timeout. Every task constructor initializes both fields, and a subsequent
wait resets the reason. Terminated tasks cannot be revived by expiration.
The existing syscall IF=0/single-core invariant keeps comparison and blocking
atomic against wakers. No user-memory access happens under TASKS.

Task layout changes do not alter assembly frame layouts: FPU saves are separately
allocated in FxArea, whose size 512 and alignment 16 remain compile-time asserted.
No hosted code, new dependency or unwinding support was added to the kernel.

## Runtime validation

TIMECHK covers relative deadlines, monotonic/realtime absolute deadlines,
zero/sub-tick waits, past deadlines, mismatch precedence, invalid signed
timespecs, zero/unsupported selective masks, explicit wakes before expiration,
expired waiter cleanup, distant INT64_MAX relative/absolute waits woken by
another thread, repeated wait/wake cycles, and glibc pthread_cond_timedwait.

The final expanded TIMECHK passes in BIOS and UEFI. Existing FUTEST
(pthread_join) and VMCHK (shared-thread mappings, permissions and rollback)
also pass. All four regression boots have no CPU exception or kernel panic.
Stable debug/release builds succeed with six existing intrinsic warnings and
no undefined linker symbols. The C fixture passes `-Wall -Wextra` syntax checks.
Runtime checks use the debug build. Review found no blocking issue in the
deadline arithmetic, task initialization, serialized completion or FPU alignment.
See [saved results and tested kernel hash](timed-futex-validation.json).

Run from WSL, one QEMU at a time:

```sh
make iso timed-futex-fixture
python3 tools/trace_jvm.py timed-final "run timechk.elf" 40
KONJAC_UEFI=1 python3 tools/trace_jvm.py timed-final-uefi "run timechk.elf" 40
python3 tools/trace_jvm.py timed-futex "run futest.elf" 25
python3 tools/trace_jvm.py timed-vm "run vmchk.elf" 40
KONJAC_GDB_EXTRA=tools/trace_unhandled_pf_gdb.py python3 tools/trace_jvm.py java-timed "run /usr/lib/jvm/java-21-openjdk-amd64/bin/java" 120
make MODE=release kernel
```

## Next Java failure and limitations

Historical checkpoint: [complete regular-file reads](large-read.md) now resolve
the class-format failure described below and record the subsequent startup faults.

Java now returns ETIMEDOUT from the previously unsupported timed wait and
reaches class loading. The recorded output is
`java/lang/ClassFormatError: Unknown constant tag 0 in class file java/lang/String`.
This establishes the next investigation target, not the cause: verify the JDK
archive bytes, the read/mapping/decompression path and the JVM's resulting class
buffer before modifying any of them. Java startup and clean process exit are
not proven; another thread continues timed waits in the bounded trace.

Selective masks/WAKE_BITSET, signals interrupting waits, robust/PI futexes and
general user-pointer validation remain unsupported. Existing wake keys compare
virtual addresses without address-space isolation; this is not a complete
Linux futex implementation. Realtime remains time since boot, with no wall-clock
adjustments. No SMP correctness or exact wake-at-deadline ordering is claimed.
