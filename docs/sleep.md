# Scheduler-backed sleep syscalls

The prior Java trace called clock_nanosleep (230) repeatedly and received ENOSYS.
SLEEPCHK reproduced this directly with `FAIL raw clock sleep errno=38`.

Linux nanosleep (35) and clock_nanosleep (230) now validate a signed x86_64
timespec and block through the scheduler until its deadline. Supported clocks
are realtime, monotonic and boottime; all currently share the existing
boot-relative PIT clock. Clock sleep accepts relative mode or TIMER_ABSTIME.
The raw nanosleep entry uses relative monotonic time. No wall clock is added.

The existing timed-futex conversion is factored into timespec_deadline so both
paths keep the same saturation, upward rounding and extra relative tick margin.
Validation rejects negative seconds and nanoseconds outside [0,999999999].
Null/overflowing request addresses get EFAULT; general user-pointer validation
remains incomplete. Unsupported clocks or flag bits return EINVAL.

The task deadline field is now wait_deadline. A common blocking helper accepts
an optional futex address; sleeps use None, so FUTEX_WAKE cannot complete them.
Expiry under TASKS makes the task runnable and clears pending metadata. Locks
are released before scheduling. The single-CPU, IRQ-disabled syscall invariant
and separate aligned FPU state are unchanged.

The tests also exposed missing sched_yield (24). Its focused check first
returned ENOSYS; dispatch now calls the existing task::yield_now and returns
zero. No lock spans that scheduling call.

Four final regression boots pass: SLEEPCHK under BIOS and UEFI, TIMECHK and
FDCHK. Each has zero CPU exceptions/kernel panics. Stable debug/release builds
pass with the six existing intrinsic warnings and no undefined linker symbols;
both timing fixtures pass `-Wall -Wextra` and the trace scripts compile. Runtime
tests use the debug kernel. Review found no blocking issue in deadline arithmetic,
blocking isolation, yield dispatch or alignment. See [saved evidence](sleep-validation.json).

## Tests

SLEEPCHK checks raw and glibc sleep entry points, relative/absolute deadlines,
all three supported clocks, past/zero/sub-tick intervals, invalid timespecs,
unaligned and untouched request buffers, unchanged remainder on success,
sibling progress and immunity to futex wakes. An initial sibling test assumed
pthread startup would finish within a short sleep; the trace showed the child
starting afterward. The fixture now waits for the child's startup handshake
before measuring progress during the sleep.
TIMECHK similarly creates its finite deadline in the initialized child rather
than before pthread_create, and starts the parent's watchdog after readiness.
This prevents debugger-heavy thread startup from consuming the entire deadline.

```sh
make iso sleep-fixture
python3 tools/trace_jvm.py sleep-final "run sleepchk.elf" 40
KONJAC_UEFI=1 python3 tools/trace_jvm.py sleep-final-uefi "run sleepchk.elf" 40
python3 tools/trace_jvm.py sleep-final-timed "run timechk.elf" 40
python3 tools/trace_jvm.py sleep-final-files "run fdchk.elf" 40
KONJAC_GDB_EXTRA=tools/trace_java_stack_gdb.py python3 tools/trace_jvm.py java-sleep-final "run /usr/lib/jvm/java-21-openjdk-amd64/bin/java" 120
make MODE=release kernel
```

The trace tool now preserves full trace/serial logs under `.work/traces/<label>`.
Sleep rows include the request timespec and a bounded user-frame-pointer walk;
untimed futex rows retain caller context for inspecting pending waits. Such walks
are diagnostic hints, not complete unwind information when frame pointers are
omitted.
`KONJAC_TRACE_SYSCALLS=0` disables per-syscall breakpoints for a lower-overhead
comparison run while retaining optional fault diagnostics. Full logs are saved
even if debugger shutdown fails; its shutdown timeout is 30 seconds.

## Java observation

The 120-second `java-sleep-final` run records six successful sleep returns,
with requests of 10 microseconds and 1 millisecond. Captured callers resolve
to HotSpot's SafepointSynchronize through VMThread. Pending futex contexts
include reference handling, signal handling, service-thread and object-monitor
waits; these contexts alone do not prove a deadlock. That traced run captures
no Java stdout/stderr. See [resolved trace evidence](java-sleep-observation.json).

A separate 60-second `java-sleep-quiet` run with `KONJAC_TRACE_SYSCALLS=0`
reaches Java launcher code and visibly prints
`java.lang.InternalError: Error loading java.security file`. The stack includes
Security.initialize and LauncherHelper.initHelpMessage. The screen is saved
at `trace-java-sleep-quiet.png`; full logs are in `.work/traces`.
Neither run records a CPU exception or kernel panic. Both virtual machines
exited after observation. This is progress, not a successful Java startup.

The next investigation is the disk JDK's configuration-file availability and
open/read behavior. On the build host, conf/security/java.security points to
/etc/java-21-openjdk/security/java.security. Whether that external target was
included in the guest disk remains unverified; inspect the disk rather than
assuming a kernel read bug or assuming the symlink explains the error.

## Limits

Signal interruption, EINTR/remainder updates, CPU-time clocks, wall-clock
adjustments and suspend accounting are not implemented. Successful sleeps leave
the remainder untouched; absolute sleeps do not use it. The clock has 10 ms
granularity and scheduling can delay return beyond the deadline. These are
limited implementations of Linux [nanosleep](https://man7.org/linux/man-pages/man2/nanosleep.2.html)
and [clock_nanosleep](https://man7.org/linux/man-pages/man2/clock_nanosleep.2.html),
not a general POSIX signal/timer subsystem.
