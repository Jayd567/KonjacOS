# JVM boot-class-path and I/O audit (2026-09-19)

Historical audit for README item 45. Item 46 subsequently fixes whole-file
buffering and the listed mapping bounds, reservation, permission and ownership
gaps. See [bounded file backing](bounded-file-backing.md) for current behavior,
runtime evidence and remaining limits; the findings below record the baseline.

## Reproduced failure and root cause

The unmodified OpenJDK 21 launcher originally printed `Failed setting boot class path.`.
A QEMU/GDB trace recorded Linux syscall arguments on entry and signed return
values at `linux_syscall_resume_frame`. Records are paired by kernel-stack
frame address, so a blocking syscall in one thread cannot be confused with
another thread's return. Non-returning exit syscalls can remain in the
reported `inflight` list; that alone does not imply a hang.

The decisive sequence was:

```text
readlink("/usr", ...)                              = -2 (ENOENT)
newfstatat(AT_FDCWD, "/lib/modules", ...)           = -2 (ENOENT)
newfstatat(AT_FDCWD, "/modules/java.base", ...)     = -2 (ENOENT)
write(..., "Failed setting boot class path.", ...) = 31
```

`/usr` exists but is not a symlink. Returning ENOENT caused glibc `realpath`
to fail on the first component. The required result is EINVAL. HotSpot uses
`realpath` to resolve its loaded libjvm location before deriving Java home;
see [OpenJDK 21u os::jvm_path](https://github.com/openjdk/jdk21u/blob/master/src/hotspot/os/linux/os_linux.cpp).
The kernel fix checks FAT16 path existence before choosing EINVAL or ENOENT.
The executable pseudo-link is preserved; zero-size readlink buffers are rejected.
This does not add general symlink support or full dirfd-relative readlinkat.

After that fix:

```text
readlink("/usr", ...) = -22 (EINVAL)
...remaining JDK path components also return EINVAL...
newfstatat(..., "/usr/lib/jvm/java-21-openjdk-amd64/lib/modules", ...) = 0
```

The original trace had 36 successful mmap calls, 11 successful read calls,
two successful pread64 calls, and no lseek calls. This rules out a returned
mmap/read error as the trigger of this particular message, not all possible
memory bugs. Its largest anonymous reservation was 128 MiB.

## Second failure: synchronous page fault while TASKS is locked

Once path resolution worked, Java stalled loading libjimage.so. Interrupting
the guest showed:

```text
IrqSpinLock<TASKS>::lock
  task::is_in_current_mmap_region
    paging::handle_page_fault (CR2 = 0x7004006000)
```

Linux read/pread copied directly to user memory inside
`with_current_open_files`. IRQ masking does not prevent synchronous page
faults. A first write to a lazy user page re-entered the same TASKS lock.
A small fixture reproduced the same lock stack with `pread` into a fresh,
untouched mmap buffer, independently of Java.

Both Linux read/pread and native int-0x80 read now stage at most 4096 bytes
in a resident kernel-stack buffer. `task::read_open_file` releases TASKS
before the caller touches user memory. No borrowed file reference escapes
the lock. No task/FPU layout or assembly boundary changed. The Linux syscall
handler's inspected stack allocation is about 4.3 KiB within the existing
32 KiB task kernel stack; the nested lazy-page-fault path passed runtime tests.

The shared staging helper also avoids constructing a slice past EOF.
Reads at/past EOF return zero, zero-byte reads don't dereference the user
buffer, and negative pread offsets return EINVAL. General validation of
untrusted user pointers is still not implemented.

## Seek support

After fixing the deadlock, Java actually issued `lseek(fd, 0, SEEK_SET)`;
it returned -38 (ENOSYS). Linux syscall 8 now supports SEEK_SET, SEEK_CUR,
and SEEK_END on buffered files. Signed addition is checked; negative or
overflowed results and unknown whence values return EINVAL without changing
the file position. Invalid descriptors return EBADF. Seeking beyond EOF is
allowed and subsequent reads return zero. SEEK_DATA/SEEK_HOLE are unsupported.

## mmap and FAT16 findings that remain open

| Area | Finding |
| --- | --- |
| File buffering | `sys_open` reads the entire file into a kernel Vec. `sys_mmap` then clones that Vec before copying to user pages. Large files can exhaust the kernel heap before any syscall errno is returned. |
| mmap bounds | Only zero length is rejected. Address/length arithmetic and file-offset additions lack overflow checks; MAP_FIXED alignment is rounded down rather than rejected. Offset alignment is not validated. |
| Invalid fd | A missing file descriptor can fall through to the anonymous-reservation path instead of EBADF. |
| Permissions | File-backed pages are initially writable/executable regardless of requested protections. Lazy anonymous faults likewise do not retain requested protection metadata. mprotect only updates already-present pages. |
| Reservation tracking | A per-task high-water mark replaces a real mapping table. Unmapped holes can be demand-filled again. Failure during eager mapping does not fully roll back allocations/frontier changes. |
| Thread ownership | Cloned tasks share page tables but retain separate mmap frontiers and open-file tables. This is not a complete Linux shared-process model. |
| FAT16 reads | ATA transfers one 512-byte sector per operation; FAT16 walks cluster chains and collects all sectors up to file size. Non-sector-aligned pread works by indexing the buffered contents. |
| Range reader | `fat16::read_file_range` already handles offset intersections with sectors, but Linux open/read/mmap do not use it. Its usize-to-u32 length conversion and truncated-chain reporting still need hardening before using it for large-file backing. |
| Short reads | Linux reads are capped at 4096 bytes, which is a valid short read. Callers must loop. The fixture checks patterned bytes across sector and cluster boundaries. |

These are audited limitations, not claims of completed mmap or filesystem
compatibility. No broad VMA rewrite was mixed into the pathname/locking fix.

## Next demonstrated blocker

With path resolution and the read deadlock fixed, Java successfully reads
and maps libjimage.so. It then kernel-panics allocating **140,848,911 bytes**,
the size of the JDK's `lib/modules` file. GDB shows
`fat16::read_file -> Vec::with_capacity -> allocation error -> panic`.
That archive is larger than the entire 96 MiB kernel heap. It is a JIMAGE
runtime archive; this failure precedes running an application jar.

The next implementation needs bounded file reads and file-backed mapping
metadata/paging, plus fallible allocation/error propagation. Merely increasing
the heap or returning success with zero-filled file pages does not fix it.
Full Java startup, `java -version`, and Minecraft remain unverified.

## Reproduction

Final regression results on the rebuilt ISO:

| Fixture | Result |
| --- | --- |
| PATHCHK.ELF | PASS readlink errors and glibc realpath |
| IOCHK.ELF | PASS seek, cross-sector pread, lazy buffers, file mmap |
| JTEST.ELF | hello from glibc dynamic |
| FUTEST.ELF | PASS futex-bitset and glibc pthread_join |

All four serial logs were checked for CPU exceptions and kernel panics:
none appeared. The six existing intrinsic-signature compiler warnings remain.
No formal proof of pointer safety, full process isolation, or all mmap
semantics is implied by these focused tests.

In WSL, with QEMU stopped:

```sh
make path-fixture io-fixture futex-fixture
make iso
python3 tools/trace_jvm.py path-check 'run pathchk.elf' 15
python3 tools/trace_jvm.py io-check 'run iochk.elf' 20
python3 tools/trace_jvm.py glibc-check 'run jtest.elf' 10
python3 tools/trace_jvm.py thread-check 'run futest.elf' 25
python3 tools/trace_jvm.py java-check 'run /usr/lib/jvm/java-21-openjdk-amd64/bin/java' 120
```

Run one VM at a time. The harness uses snapshot mode to preserve disk.img,
reads the matching kernel's symbols, and stops its own QEMU/GDB processes.
It requires QEMU, GDB with Python, and Pillow. Logs are in
`/tmp/konjac-<label>/`; screenshots are `trace-<label>.png` in the repository.
The harness exiting successfully means collection finished: inspect guest
PASS/FAIL output and serial diagnostics to determine the actual result.
Fixtures use hosted glibc for compilation, not for kernel linkage. IOCHK
also calls KonjacOS's native ABI and must not be run as a host Linux test.
The fixture targets add files to the existing FAT disk without regenerating
it or removing the JDK installed directly on that disk.
