# Bounded file backing implementation plan

Goal: remove the demonstrated JDK modules archive heap panic, with QEMU evidence.

Design: FAT16 open produces a small copyable cluster/size handle. Reads fill a
resident caller buffer; mmap records retain their own handle after close.
An IRQ-safe, fixed-capacity mapping table keyed by CR3 owns reservations and
protections for all threads sharing that address space. Faults populate one
physical page through the HHDM, with no TASKS lock or user pointer held during I/O.
Metadata exhaustion returns ENOMEM without changing existing mappings. File
mappings are private; shared writeback and mutation of open FAT files are outside
this milestone. The latter remains an explicit filesystem lifetime limitation.

Alternatives considered: streaming into eager mappings removes heap copies but
still consumes physical memory proportional to the archive; a general page cache
and VFS adds unnecessary scope. Demand paging with bounded records is selected.

Constraints: stable no_std Rust, panic=abort, existing ABI and FPU layout,
96 MiB heap, existing Makefile and WSL/QEMU toolchain. No new dependencies.
This checkout has no Git metadata, so edits are made directly with local backups.

- [x] Add BIGCHK guest fixture and reproduce the old archive-open panic.
- [x] Add FAT16 handle/range reads; wire both syscall ABIs, fstat and lseek.
- [x] Add shared reservation metadata, fault population, split/unmap/protection
      operations, teardown, and fallible page-table installation.
- [x] Test invalid mmap arguments, partial replacement/unmap, protection of
      untouched pages, closed descriptors, shared-thread visibility and repeats.
- [x] Build kernel/ISO; run BIGCHK, IOCHK, PATHCHK, JTEST and FUTEST in QEMU.
- [x] Trace the real Java launcher, inspect exceptions/panics and document the
      actual next blocker. Review safety invariants and update README/audit.

Runtime evidence additionally required correcting page-fault stack alignment,
nested FPU saves and fatal dispatch, as documented in docs/bounded-file-backing.md.

