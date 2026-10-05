# Complete regular-file reads with bounded staging

The captured Java failure was `Unknown constant tag 0 in class file
java/lang/String`. The disk-image modules archive and the host JDK archive
both have SHA-256
`9a8e9e8a7aab3a5e010e56c65f13449c17b08e5eede93cd7bb796b7df31c572b`.
The host `jimage verify` command accepts the extracted archive.

The syscall trace narrowed the failure to pread at offset `0x223cd9`, requesting
`0xc002` (49,154) bytes and receiving only 4,096. This archive slice starts with
CAFEBABE and has FNV-1a-32 fingerprint `0x4f7337c4`. The kernel had incorrectly
used its staging-buffer size as the total regular-file transfer limit. Although
short reads are possible in Linux, this artificial limit prevented the tested
libjimage path from receiving the available class bytes.

## Change and invariants

Linux read and pread now share a loop using one 4 KiB stack buffer. They keep
reading/copying chunks until the requested count, EOF or an error. The request
is capped at `0x7ffff000`; destination arithmetic is checked for overflow.
No allocation scales with the file or request size. Each disk read completes
outside TASKS and each user copy happens after the helper releases TASKS, so
untouched destination pages can demand-fault safely.

pread advances its explicit offset per chunk without changing ordinary file
position. read advances ordinary position. A later I/O error returns the bytes
already copied; an initial error returns the error. Zero-length requests still
validate the descriptor through the backing helper. Native int-0x80 reads and
write syscalls retain their existing limits.

## Focused regression

READCHK failed before the fix because a 49,154-byte request returned only 4 KiB.
It passes after the fix, including the exact JDK String slice fingerprint,
unaligned untouched destination pages, pattern checks across chunks, sequential
position, pread position preservation, large requests ending at EOF, partial EOF,
zero-length reads and invalid descriptors.

Reproduce from WSL in the repository, one QEMU at a time:

```sh
make iso large-read-fixture
python3 tools/trace_jvm.py read-after "run readchk.elf" 35
KONJAC_UEFI=1 python3 tools/trace_jvm.py read-uefi "run readchk.elf" 35
python3 tools/trace_jvm.py read-io "run iochk.elf" 30
python3 tools/trace_jvm.py read-big "run bigchk.elf" 40
KONJAC_GDB_EXTRA=tools/trace_unhandled_pf_gdb.py python3 tools/trace_jvm.py java-read "run /usr/lib/jvm/java-21-openjdk-amd64/bin/java" 120
make MODE=release kernel
```

## Following startup fix: getcwd

After the read correction, Java loads classes and reaches SystemProps, but
reports that it cannot determine the current working directory. Its trace
records syscall 79 returning ENOSYS. Linux getcwd now reports the existing
filesystem CWD as an absolute NUL-terminated path. The raw return value counts
the NUL; insufficient space returns ERANGE without writing. Null/overflowing
output pointers return EFAULT, with the broader trusted-pointer limitation
unchanged. Paths exceeding 4096 bytes return ENAMETOOLONG. The owned path is
built before copying, so no CWD lock spans a user fault.

CWDCHK first fails with ENOSYS, then passes raw length, terminator/guard bytes,
short/zero buffers, lazy destinations and glibc's allocating getcwd wrapper.
The libc allocation behavior is described in the
[Linux getcwd documentation](https://man7.org/linux/man-pages/man3/getcwd.3.html).

```sh
make getcwd-fixture
python3 tools/trace_jvm.py cwd-final "run cwdchk.elf" 25
python3 tools/trace_jvm.py cwd-subdir "cd /usr/lib
run EXPECT_CWD=/USR/LIB /cwdchk.elf" 25
KONJAC_GDB_EXTRA=tools/trace_unhandled_pf_gdb.py python3 tools/trace_jvm.py java-cwd "run /usr/lib/jvm/java-21-openjdk-amd64/bin/java" 120
```

The CWD remains global, not per-process; chdir/fchdir syscalls are not added.
Short FAT names use the filesystem's canonical uppercase spelling: the first
non-root test expected lowercase `/usr/lib` and failed; expecting `/USR/LIB`
matches the existing filesystem behavior and passes without a kernel change.
Review also noted a pre-existing concurrency risk: CWD's plain spinlock is used
by preemptible kernel callers and IRQ-disabled filesystem syscalls. If a holder
is preempted, such a syscall can spin waiting for a task unable to run. This
needs a separate lock-path review; the serial guest tests do not prove safety
against concurrent shell directory changes.

## Latest Java result

Historical checkpoint: [shared thread descriptors](clone-files.md) now resolve
the failed child reads that preceded this recursive class-resolution fault.

Six final regression boots pass: READCHK in BIOS and UEFI, IOCHK, BIGCHK,
CWDCHK at root (including NULL/overflow raw pointers), and CWDCHK after shell
cd to /usr/lib. Those boots have no CPU exception or kernel panic. Debug and
release builds succeed with six existing intrinsic warnings and no unresolved
symbols. Both new C fixtures pass `-Wall -Wextra` syntax checks. Runtime tests
use the debug kernel. See [saved results and kernel hash](large-read-validation.json).

The final trace (`java-cwd`) records the String pread returning 49,154 and
getcwd returning 2 for `/`. Neither earlier initialization error recurs.
The next fault is a user write (error 0x6) at RIP `0x70011998f1`, CR2
`0x707c003fe8`, RSP `0x707c003f90`. The trace previously maps and protects
`0x707c000000..0x707c004000` with PROT_NONE. libjvm's recorded load base is
`0x7000245000`; addr2line for relative PC `0xf548f1` names
`SystemDictionary::resolve_instance_class_or_null`. Task 9 and its thread group
are killed while the kernel remains scheduled. The stack-guard fault is a new
investigation target; its underlying cause is not established, and increasing
the stack or weakening protection is not justified by this evidence alone.

## Remaining limits

Existing trusted-pointer assumptions remain; this is not general Linux EFAULT
handling. Descriptor tables are still per-task. Disk reads remain synchronous
with interrupts disabled, so very large transfers delay scheduling. Later-chunk
I/O-error handling was reviewed but has no injected-error runtime test.
The class fingerprint is specific to the installed JDK archive hash above.
