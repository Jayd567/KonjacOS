# Bounded file backing (2026-09-19)

## Implemented behavior

Linux and native open/read now retain a small FAT16 cluster/size handle instead
of loading the entire file into a kernel Vec. Reads fill at most 4096 bytes of
resident kernel buffer and release TASKS before disk I/O or user-memory access.
fstat and lseek use the handle's size. Synthetic proc files retain static bytes.
The FAT range helper checks lengths before conversion, uses fallible allocation,
and reports truncated/invalid chains encountered while reading. Each handle
caches its forward cluster position; each read caches one FAT sector.

`vm.rs` holds at most 2048 mapping records globally, keyed by address-space CR3.
Both syscall ABIs use these reservations; cloned threads see the same mappings.
File mappings retain independent backing identities, including file offsets,
after descriptor close. A first access allocates one physical frame, fills it
through the HHDM, then publishes its PTE with the requested permissions. The
last partial file page is zero padded; fully beyond-EOF pages are not supplied.
No allocation scales with the size of a mapped file. The heap remains 96 MiB.

Partial unmaps, fixed replacements and protection changes split reservations
while preserving file offsets. Removed holes cannot silently become anonymous
memory again. Missing descriptors, misalignment and arithmetic overflow fail
instead of being accepted as unrelated mappings. Read-only shared mappings
cannot be upgraded to writable mappings. Metadata capacity is checked before
mutation; page-table allocation failure frees intermediate tables created by
that attempt and the unpublished data frame. The last thread-group reaper
removes the address space's reservation records as well as its page tables.

## Invariants exposed by the runtime tests

- Page-fault entry must align RSP before calling Rust. The expanded fault path
  reproduced a kernel #GP on `movaps [rsp+0x40]` with the old entry alignment.
- A page fault needs its own aligned 512-byte FXSAVE buffer on the kernel stack.
  Reusing the per-task slot overwrites an outer syscall's saved user FP state.
  Task FxArea remains explicitly 16-byte aligned and 512 bytes, checked at compile
  time. No syscall frame/register layout or external calling convention changed.
- Fatal page-fault dispatch must pass all six arguments to exception_handler,
  including saved CS, the register frame and user RSP. OOMCHK reproduced a
  secondary diagnostic fault with the incomplete old call. Diagnostics now read
  only present user pages through HHDM and never demand-fault while printing.
- IRQ-off faults/reaping must not spin on a lock held by a preempted task. FAT
  layout and PMM use IRQ-safe guards; ATA commands are indivisible transactions.
  TASKS may acquire the mapping lock during teardown; mappings never acquire
  TASKS. No user pointer is accessed while holding the mapping lock.

## Regression probes

`make largefile-fixture vm-fixture io-fixture path-fixture futex-fixture` adds
fixtures to the existing disk without regenerating it or removing the JDK.

- BIGCHK: real 140,848,911-byte modules archive; reads at both ends and beyond
  4 GiB, mapping after close, EOF padding, four repeated map/unmap cycles, and
  physical backing bounded below 1 MiB rather than the full archive size.
- VMCHK: invalid fd, alignment/overflow, middle-page replacement, preserved
  file offsets, untouched-page protection, holes, shared-write rejection and
  parent/child mapping visibility, metadata exhaustion, failed-edit rollback
  and reuse of released slots.
- IOCHK: existing read/seek/EOF/offset tests plus SIMD state preservation across
  a syscall copying into a previously untouched page.
- OOMCHK: reserves/touches 512 MiB on a 256 MiB VM. Expected outcome is a ring-3
  page fault, process-group termination and scheduler recovery, not PASS text.
  `KONJAC_AFTER_FAULT="run bigchk.elf"` then verifies archive reads/mappings
  succeed in the same boot after the process's frames are reclaimed.
- PATHCHK, JTEST and FUTEST retain the earlier pathname, dynamic-glibc and
  pthread-join coverage.

The original BIGCHK failed with the exact archive-sized allocation panic.
The old mapping path failed VMCHK's invalid-descriptor check; an intermediate
version also failed the shared-write upgrade check before that was corrected.

Final results: BIGCHK, VMCHK, IOCHK, PATHCHK, JTEST and FUTEST pass in BIOS
QEMU with no CPU exception or kernel panic. BIGCHK also passes under UEFI.
The intentional OOM run has exactly one ring-3 exception, followed by a passing
BIGCHK in the same boot. Debug and release builds succeed on stable Rust 1.98.1
with six pre-existing intrinsic warnings; neither ELF has unresolved symbols.
Runtime tests use the debug build. See [saved evidence](bounded-file-validation.json)
for the tested kernel hash and guest success messages.

Run one VM at a time using `tools/trace_jvm.py`. `KONJAC_UEFI=1` selects the
existing OVMF firmware files with disposable variable-store writes. Optional
`KONJAC_GDB_EXTRA=tools/trace_unhandled_pf_gdb.py` records faults rejected by
the demand-paging path. The trace collector now tolerates nonresident write
buffers at syscall entry and retries the diagnostic read after syscall return.

## Next JVM blocker

Historical checkpoint below; [startup ABI fixes](jvm-startup-abi.md) now resolve
the getcpu failure and record the newer timed-futex blocker. That work also
corrects sysinfo.mem_unit: the original BIGCHK physical-memory comparisons
were ineffective with a zero unit. Its new baseline guard and rerun validate
the byte-count bounds. Other earlier functional checks remain valid.

The real launcher successfully opens/stats/maps `lib/modules` instead of
panicking. The first recorded unhandled page fault has RIP and CR2
`0xffffffffff600800`, saved CS `0x4b`, and error `0x14` (user instruction fetch
from a nonpresent page). This is consistent with the legacy Linux getcpu
vsyscall entry. The trace also records syscall 309 returning ENOSYS.

HotSpot subsequently reports SIGSEGV and eventually aborts; the kernel remains
alive. Its printed `pc=0` is not the captured hardware RIP: KonjacOS's existing
signal delivery does not supply a full Linux siginfo/ucontext. Investigate the
getcpu/vsyscall path and signal ABI next. Java startup, java -version and Java
application execution remain unverified; shell argument support is still pending.

## Remaining limits

This is not a general Linux VM/VFS implementation. Backing files must remain
unchanged while open or mapped: FAT unlink/overwrite pinning and coherent page
caching are not implemented. Writable shared mappings are rejected. Descriptor
tables and brk remain per-thread. Address hints and several mmap flags remain
unsupported; the fixed record table can return ENOMEM and does not coalesce
adjacent records. Empty page tables are reclaimed at address-space teardown.

Beyond-EOF/I/O failures use the existing SIGSEGV/fatal path, not Linux SIGBUS.
Demand-allocation failure kills the process; unrelated eager loader/brk/heap
paths still have their previous allocation limitations. User-pointer validation,
complete signal contexts, full FAT cycle/corruption handling and SMP are outside
this change. Review verified lock order and ABI invariants; no active clangd or
Semgrep tool was available, and the rust-analyzer shim had no installed binary.
