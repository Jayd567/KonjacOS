# Java startup: CPU queries and memory reporting

The production changes are confined to `kernel/src/linux_syscall.rs`:

- Dispatch Linux syscall 309 to getcpu. Optional outputs are unsigned 32-bit
  CPU/node values, both zero for the current boot-CPU-only scheduler. Unaligned
  writes are permitted; no lock spans user writes that may demand-fault. The
  obsolete cache argument is ignored. Existing trusted user-pointer assumptions
  remain; invalid addresses are not converted into Linux EFAULT.
- Write sysinfo.mem_unit at byte 104, not 100. x86_64 alignment places totalhigh
  at 88 and freehigh at 96. The old write left the unit zero and set freehigh
  to 4294967296. Total/free RAM are already reported in bytes, so the unit is 1.

## Evidence and tests

CPUCHK first failed with ENOSYS (errno 38), then passed direct syscall and
glibc sched_getcpu checks, optional pointers, surrounding guard bytes,
unaligned stores and outputs on two untouched demand-mapped pages.

SYSCHK compiles against actual glibc headers with static assertions for the
112-byte structure and mem_unit offset 104. Before correction it printed
`FAIL sysinfo memory units: unit=0 freehigh=4294967296`; afterward it passes
the unit, high-memory fields, memory-value sanity and surrounding guards.

BIGCHK now rejects a zero memory unit or free-memory baseline. Previously its
physical-memory comparisons multiplied by zero and could pass trivially.
The corrected guest rerun passes all four archive cycles with real byte counts.
The older bounded-file evidence remains historical and must not be cited as
proof of those physical-memory bounds without this correction.

Reproduce from WSL in the repository, one QEMU at a time:

```sh
make iso getcpu-fixture sysinfo-fixture largefile-fixture
python3 tools/trace_jvm.py startup-cpu "run cpuchk.elf" 25
python3 tools/trace_jvm.py sysinfo-after "run syschk.elf" 25
python3 tools/trace_jvm.py sysinfo-big "run bigchk.elf" 40
KONJAC_UEFI=1 python3 tools/trace_jvm.py startup-sysinfo-uefi "run syschk.elf" 25
KONJAC_UEFI=1 python3 tools/trace_jvm.py startup-big-uefi "run bigchk.elf" 35
python3 tools/trace_jvm.py startup-futex "run futest.elf" 25
python3 tools/trace_jvm.py startup-io "run iochk.elf" 25
KONJAC_GDB_EXTRA=tools/trace_unhandled_pf_gdb.py python3 tools/trace_jvm.py java-sysinfo "run /usr/lib/jvm/java-21-openjdk-amd64/bin/java" 90
make MODE=release kernel
```

## Next blocker

This is the historical checkpoint before [timed futex support](timed-futex.md),
which resolves that abort and records the newer class-loading failure.

Final verification: all seven fixture runs listed above pass without CPU
exceptions or kernel panics. Debug and release builds succeed with the six
existing intrinsic warnings and no undefined linker symbols; fixture C files
also pass `-Wall -Wextra` syntax checks. Runtime evidence uses the debug kernel.
CPUCHK additionally passed UEFI before the independent sysinfo correction.
See [saved results and tested kernel hash](jvm-startup-validation.json).
Review found no blocking issue in the output widths, alignment or sysinfo layout.

The getcpu-only Java trace returns zero from syscall 309 and replaces the
legacy-address crash with `Too small maximum heap`. Correcting sysinfo then
gets past that error and reaches creation of another thread (task 7).
That thread calls futex with op 0x89 (private WAIT_BITSET), a non-NULL timeout
and MATCH_ANY. The current implementation explicitly rejects timed waits with
ENOSYS. The guest screen then reports the futex facility's unexpected-error
message; abort tries unsupported tgkill and ultimately executes HLT at
0x70000579a2, producing a ring-3 general-protection fault and group termination.
The kernel remains scheduled. This fault is downstream of the failed futex,
not evidence that the CPU-query or memory-unit fixes failed.

Next work should implement timed futex waits with deadline/clock semantics,
timeout return codes and race tests, then rerun the actual launcher. Full
signal contexts, shell arguments, java -version and application execution
remain unverified/incomplete.

The legacy fallback diagnosis also agrees with
[OpenJDK's Linux CPU-query initialization](https://github.com/openjdk/jdk21u/blob/master/src/hotspot/os/linux/os_linux.cpp):
it tests libc sched_getcpu and falls back to the legacy entry if unavailable.
No legacy executable page is required for the tested startup path.
