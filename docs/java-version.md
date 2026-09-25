# Real `argv[1..]`, and `java -version` prints the real banner

## The blocker

Item 51 (`sleep.md`) left off at a real 60-second `java` trace reaching
`LauncherHelper` and printing `Error loading java.security file` -- next
step named there was "inspect the disk JDK's configuration files and their
open/read paths," since the host's `conf/security/java.security` is a
symlink to `/etc/java-21-openjdk/security/java.security` and it was
unverified whether that external target had ever made it onto the guest
disk.

It hadn't: this session's `disk.img` is gitignored and ephemeral (rebuilt
fresh by `make disk` from `disk_root/`, which has no JDK), so the JDK/glibc
placement every item back to 34 relied on doesn't survive between sessions
by design -- it's re-done by hand each time, "any JDK installed directly
into it" per the `Makefile`'s own `disk` target comment.

## Rebuilding the disk placement

Same approach as item 34: real files at their real absolute paths, symlinks
dereferenced (FAT16 has none), no `DT_NEEDED`/`PT_INTERP` string patching.

```sh
# JDK tree, symlinks resolved to real files at the same relative paths
# (conf/security/java.security, lib/security/cacerts, lib/jvm.cfg, ... all
# point outside the JDK tree on a real Debian/Ubuntu install -- rsync -L
# bakes their real content in at the JDK-relative path instead)
rsync -rL --exclude=jmods --exclude=legal --exclude=man --exclude=include --exclude=docs \
  /usr/lib/jvm/java-21-openjdk-amd64/ staging/java-21-openjdk-amd64/
mmd -i disk.img ::/usr ::/usr/lib ::/usr/lib/jvm
mcopy -s -i disk.img staging/java-21-openjdk-amd64 ::/usr/lib/jvm/

# ld.so itself + the handful of DT_NEEDED libraries `java`/libjvm.so use,
# at ld.so's real compiled-in default search paths (item 31's own finding:
# a DT_NEEDED name with no '/' is searched against a fixed list of default
# directories, not the FAT16 root)
mmd -i disk.img ::/lib64 ::/lib ::/lib/x86_64-linux-gnu
mcopy -i disk.img /lib64/ld-linux-x86-64.so.2 ::/lib64/
mcopy -i disk.img /lib/x86_64-linux-gnu/{libc.so.6,libz.so.1,libstdc++.so.6,libm.so.6,libgcc_s.so.1} \
  ::/lib/x86_64-linux-gnu/

# /etc/passwd (needed for glibc's own getpwuid() -- see "Confirming the
# lead" below) and an empty /tmp for HotSpot's PerfData directory
mmd -i disk.img ::/etc ::/tmp
echo "root:x:0:0:root:/root:/bin/sh" > /tmp/passwd_stage
mcopy -i disk.img /tmp/passwd_stage ::/etc/passwd
```

`jmods`/`legal`/`man`/`include`/`docs` are excluded -- 81 MiB of module
sources and documentation `java`'s own startup never opens, not needed to
reproduce this. The staged tree is ~205 MiB; the two symlinks with no
referent on this machine (`lib/libatk-wrapper.so`, `lib/src.zip` -- GTK/
source-bundle integration this install never actually has) are skipped by
`rsync -L`, harmlessly: `java -version` never opens either.

## The second blocker this immediately exposed: no real `argv[1..]`

With the disk placement done, `run /usr/lib/jvm/java-21-openjdk-amd64/bin/java -version`
failed immediately with `no such file or directory` -- not a real ENOENT
from the loader, but `cmd_run` (`commands.rs`) treating the entire
remainder of the line, `.../bin/java -version`, as one literal path.
`cmd_run` never split anything past the path; `loader.rs`'s
`build_initial_stack` only ever wrote a single-element `argv` (`argc=1`,
`argv[0]`, `NULL`) for every caller, matching the README's own
"still-open gap" callout on this exact point (see item 41's `envp` entry
and the roadmap's own "the shell also still needs arguments" line).

Fixed in `loader.rs`/`commands.rs`:

* `build_initial_stack` now takes a full `argv: &[String]` instead of a
  single `argv0: &str`, writes every argument's real NUL-terminated string
  onto the stack (same "strings at the top, pointer array below" layout
  `envp` already used, argv written first so envp continues right below
  it), and pushes a real `argc`/`argv[]` pointer array sized to match --
  `ptr_words`' accounting grew from a hardcoded `2` (`argv[0]` + NULL) to
  `argv_addrs.len() + 1`.
* `cmd_run` now splits whatever follows the path on whitespace into real
  `argv[1..]`, after the existing `VAR=value` prefix parsing for `envp`
  (unchanged) -- `run java -version` now hands the spawned task a real
  `argv` of `["java", "-version"]`, `argv[0]` being the same path string
  `argv0` always was.
* Both `load_and_run` and `load_and_run_with_interp` (the `PT_INTERP`
  dynamic-linking path `java` itself goes through) take the new `argv`
  slice through to `build_initial_stack`; `task::spawn_user`'s separate
  `exe_path` display string is unaffected (still `argv[0]` alone).

No parsing beyond plain whitespace-splitting (no quoting, no escaping) --
enough for `-version`/`-jar foo.jar`-shaped real invocations, not a real
shell's word-splitting.

## Result

Verified in QEMU (BIOS), `KONJAC_TRACE_SYSCALLS=0` (matching item 51's
"quiet" methodology -- the per-syscall GDB breakpoints add enough overhead
to matter over a ~90-second JVM run):

* `run hello.exe` (PE32+, `argv[1..]` empty) still prints its existing
  message -- the `argv` plumbing change is a genuine no-op for every
  existing zero-extra-argument caller.
* `run /usr/lib/jvm/java-21-openjdk-amd64/bin/java -version` now prints,
  to the real framebuffer console, byte-for-byte what this same JDK prints
  on the host:

  ```
  openjdk version "21.0.10" 2026-01-20
  OpenJDK Runtime Environment (build 21.0.10+7-Ubuntu-124.04)
  OpenJDK 64-Bit Server VM (build 21.0.10+7-Ubuntu-124.04, mixed mode, sharing)
  ```

  No CPU exception, no kernel panic, across two independent runs (90s and
  100s) with identical final screen state both times. Screen saved at
  [`trace-java-version.png`](../trace-java-version.png).

This is the exact milestone the roadmap's item 1 named as still open
("Startup still needs a successful end-to-end run; full VM startup and
`java -version` remain unverified") -- now verified, not by patching
around anything, but by finishing the disk placement item 51 already
pointed at and closing the real `argv[1..]` gap it happened to expose.

## Closing more of the gap: eight more real syscalls

The first `java -version` run (above) didn't reach a clean `exit_group`:
after the banner, HotSpot's own background compiler/GC/sweeper threads
each hit several syscalls this kernel didn't implement --
`273` (`set_robust_list`), `334` (`rseq`), `302` (`prlimit64`), `96`
(`gettimeofday`), `157` (`prctl`), `229` (`clock_getres`), `107`
(`geteuid`), `41` (`socket`) -- and the console's final state was
identical across a 90s and a separate 100s run, no process-exit ever
observed.

Implemented in `linux_syscall.rs`, all following the module's existing
"real value where this kernel has one, honest refusal where it doesn't"
discipline:

* `gettimeofday`/`clock_getres` -- same boot-relative `timer::ticks()`
  source `clock_gettime` already used; `clock_getres` reports this
  kernel's real 10ms (100Hz) tick granularity, not a fabricated
  high-resolution value.
* `getuid`/`geteuid`/`getgid`/`getegid` -- `0`, matching the `AT_UID`/
  `AT_EUID`/`AT_GID`/`AT_EGID` auxv entries `loader.rs` already reports;
  this kernel has no real user/permission model to back up anything else.
* `prctl` -- accepted no-op (every glibc thread calls `PR_SET_NAME` once
  at startup; nothing here reads thread names back yet).
* `set_robust_list` -- accepted, not acted on (this kernel's futex/task-
  death paths don't walk the registered list; every glibc thread calls
  this once, unconditionally, so refusing it outright was pure noise).
* `prlimit64` -- honest real numbers for `RLIMIT_NOFILE` (matches
  `task::MAX_OPEN_FILES`) and `RLIMIT_STACK` (8 MiB soft/unlimited hard,
  real Linux's own common default); unlimited for everything else;
  *setting* a new limit is refused (`ENOSYS`) since nothing here enforces
  limits for a new one to change.
* `rseq` -- explicit `ENOSYS` (same value the catch-all already gave it,
  just named so it stops logging once per thread): real per-context-
  switch `cpu_id` maintenance isn't implemented, and modern glibc already
  treats a failed registration as "unavailable" and falls back cleanly.

Verified: `run hello.exe` unaffected. `run
/usr/lib/jvm/java-21-openjdk-amd64/bin/java -version`'s own visible
output is unchanged (same real banner), but the console noise after it
dropped from a long stream of unimplemented-syscall lines spanning eight
different thread addresses down to three total (`137` `statfs`, `41`
`socket` x2, both from what's almost certainly `AttachListener` probing
its `/tmp` attach-socket path once) -- consistent with most of HotSpot's
own thread-startup bookkeeping now actually succeeding instead of
silently failing and retrying. Screen saved at
[`trace-java-exit.png`](../trace-java-exit.png).

## Still not a clean `exit_group`

A separate run with the GDB syscall tracer left on (`KONJAC_TRACE_SYSCALLS=1`,
the default) caught the actual steady state directly instead of inferring
it from console silence: at the 60-second mark, three futex waits are
still pending --

```
{"n": 202, "args": ["0x7001c34990", "0x109", "0x6", ...]}   // timed FUTEX_WAIT_BITSET
{"n": 202, "args": ["0x700176bc10", "0x89",  "0x0", ...]}   // untimed FUTEX_WAIT_BITSET
{"n": 202, "args": ["0x700176ce90", "0x89",  "0x0", ...]}   // untimed FUTEX_WAIT_BITSET
```

and the trace log up to that point shows the same three waits repeatedly
expiring (`ret: -110`, `ETIMEDOUT`) and immediately being reissued --
consistent with HotSpot's real `WatcherThread`/safepoint-polling steady
state (see item 51's own README entry: "six successful sleeps in JVM
safepoint synchronization" was this same loop, observed earlier in
startup). No `exit`/`exit_group` (syscalls 60/231) appears anywhere in
either trace. This is real, ordinary HotSpot background-thread behavior,
not a hang this kernel is directly causing -- but real `java -version` on
real Linux reaches `JNI_DestroyJavaVM` and tears these threads down
within milliseconds of printing the banner, and this kernel's guest never
gets there within a 60-second observation window.

## Confirming the lead: `/etc/passwd`, `mkdir`, and real further progress

The `mkdir`/`hsperfdata` divergence above was a real, testable hypothesis,
not just a plausible-sounding guess -- confirmed cheaply, in two steps,
*before* writing any kernel code:

1. Added a minimal `/etc/passwd` (`root:x:0:0:root:/root:/bin/sh`) and an
   empty `/tmp` directory directly to `disk.img` via `mmd`/`mcopy` (no
   kernel change at all). Re-running the exact same `java -version` trace
   immediately surfaced a *new* unimplemented-syscall line that had never
   appeared before: `83` (`mkdir`, `a1=0x1ed` i.e. mode `0755`) --
   confirming glibc's `getpwuid()` (needed to build the real
   `hsperfdata_<user>` directory name) really was the earlier, silent
   failure point, exactly as the host `strace` evidence suggested.

2. Implemented real `mkdir(2)`/`mkdirat(2)` (`linux_syscall.rs`, onto a
   new `fat16::create_dir`): allocates one cluster, zero-fills it, writes
   real `.`/`..` entries (this driver had directory *reading* since item
   34 but never directory *creation* -- file writing, item 5, only ever
   needed to place an entry in a directory that already existed), then
   adds one `ATTR_DIRECTORY` entry in the parent. `mkdirat`'s `dirfd` is
   ignored, same precedent `openat` already set: every path asked for
   here has been absolute.

With both, `mkdir` no longer appears as "unimplemented" at all -- it
genuinely succeeds. And the effect goes well past that one call: a
syscall-traced run afterward shows **eight** pending futex waits instead
of the previous run's three, several with real, deep HotSpot call stacks
(compiler-broker/class-loading frames, not just the shallow safepoint
loop from before) and at least one real `FUTEX_WAKE` actually waking a
waiter (`ret: 1`) -- concrete evidence of more of HotSpot's own thread
pool genuinely starting up and synchronizing, not just retrying the same
three waits forever. `-version`'s own visible output is unaffected (same
real banner); `run hello.exe` is unaffected. Still no observed
`exit_group` within the trace windows tried so far -- the process is
doing more real, further-along work now, not necessarily less time from
finishing it.

## Building the rest of the `hsperfdata` chain

The `mkdir`/`hsperfdata` lead above named `mkdir` as the first divergence,
but real HotSpot's own PerfData setup needs much more than that one call
once it succeeds -- confirmed against a fresh host `strace -f java
-version` (with `/tmp/hsperfdata_root` removed first, to match this
kernel's own always-fresh disk): `openat(O_NOFOLLOW)` to check for an
existing directory, `mkdir`, `openat(O_NOFOLLOW)` again, `fstat`,
`geteuid`, `openat(O_DIRECTORY)`, more `fstat`s, `openat(".")` to save the
caller's own cwd, `fchdir` into the new directory, `openat(O_CREAT)` for a
file named after the PID, `fstat`, `fcntl`, `fchdir` back, `flock`,
`ftruncate` (twice), eight `lseek`+`write` pairs (one byte per 4 KiB
page, forcing real page allocation), and finally `mmap(MAP_SHARED)` on
that same fd -- all before any class loading starts.

Implemented in this kernel for the first time (`linux_syscall.rs`,
`task.rs`, `fat16.rs`), rather than continuing to patch around each one:

* **Real writable regular files.** `task::OpenFile` grows a new `extra`
  field (`Option<Box<OpenExtra>>`) carrying either an open directory's
  resolved path (`OpenExtra::Dir`) or a real file's full content plus its
  path (`OpenExtra::Writable`) -- kept separate from the existing
  `FileBacking` enum deliberately, since `FileBacking` has to stay `Copy`
  for `vm.rs`'s own fixed-size `Region` array (2048 entries, no
  allocation) to keep working unmodified; `Vec<u8>`/`String` variants
  would have broken that. `sys_open`/`sys_openat` now honor `O_CREAT`
  (materializing a new empty file in memory) and a write-mode open of an
  existing file (reading it fully into memory first) -- a plain read-only
  open of a regular file is completely unchanged, still the original
  lazy/bounded `FileBacking::Disk` path items 46/49 built.
* **Real `write`/`pwrite`, routed by fd.** `sys_write` previously wrote
  *every* call straight to the console regardless of `fd`, harmlessly
  only because nothing had ever opened a real file for writing before.
  Now `fd` 0/1/2 still go to the console; anything else routes to
  `task::write_open_file`, extending the in-memory buffer with zero
  bytes on a write past the current end, same as a real sparse write.
* **`mkdir`/`mkdirat`**, onto a new `fat16::create_dir` -- see the
  section above.
* **`fchdir`**, onto `fat16::change_dir` using the open directory fd's
  own resolved path -- this filesystem's `CWD` was already a single
  global before this (item 49's own "this does not add per-process CWD"
  callout), so reusing it here is the same honestly-narrower-than-real-
  Linux scope, not a new limitation.
* **`ftruncate`**, growing or shrinking a `Writable` fd's in-memory
  buffer.
* **`flock`**, accepted as a no-op -- there is never a second process
  here to actually contend with.
* **A real `MAP_SHARED`+`PROT_WRITE` `mmap` onto a `Writable` fd.**
  `vm.rs`'s general file-backed mapping path explicitly refuses this
  combination already (`"Shared writable pages require cache/writeback
  semantics we do not have"`) -- correctly, for the general case. But
  there's never a second reader for this kernel's own writable fds to
  share memory *with* either, so `sys_mmap` special-cases a `Writable` fd:
  reserve an ordinary anonymous region through the existing `vm::map`
  (unmodified address allocation, region bookkeeping, and process-
  teardown frame cleanup), eagerly populate every page through the exact
  same `vm::fault` a lazy first touch would have used, then copy the fd's
  current buffer content in.

**Verified:** `mkdir` now genuinely creates a real, persistent FAT16
subdirectory -- checked directly with `mdir` against a non-snapshotted
disk image, not just inferred from console silence -- and neither
`hello.exe` nor `java -version`'s own banner regressed. A targeted GDB
breakpoint on `linux_syscall_handler` (filtering for syscall numbers 83/
81/77/73, real `mkdir`/`fchdir`/`ftruncate`/`flock`) confirms `mkdir`
really does fire during a live `java -version` run.

**Not yet established:** whether the *rest* of the chain (`fchdir`
onward) actually runs. `mkdir` firing is confirmed; `fchdir`/`ftruncate`/
`flock` were not observed firing in the trace windows tried so far, and a
non-snapshotted disk check after a full run shows the directory created
but still empty -- no per-PID file, meaning the dance is abandoned
somewhere between `mkdir` succeeding and the file actually being created,
not completed. Whether that's a real remaining bug in the new code above,
or real HotSpot choosing to abandon PerfData for an unrelated reason
(there's real precedent for this being silent and non-fatal on real Linux
too) isn't established yet. Full syscall tracing is heavy enough here
(each breakpoint hit costs a real GDB round-trip) that reaching this
point in HotSpot's startup reliably within one bounded trace window has
been inconsistent run to run -- the next real step is a longer or more
targeted trace, not a new hypothesis.

**Not yet established (unchanged from before):** what specifically real
HotSpot's shutdown path
needs from this kernel that it isn't getting. A real lead, though, from
`strace -f java -version` on the host (the same free-ground-truth
technique every prior item in this chain has used) -- real shutdown's
last ~20 syscalls, across several threads:

```
unlink("/tmp/hsperfdata_root/<pid>")       # PerfData shared file cleanup
futex(..., FUTEX_WAKE_PRIVATE, N)          # main thread wakes worker threads
rt_sigprocmask(...)                        # each worker unblocks its signal mask
gettid()
futex(..., FUTEX_WAIT_PRIVATE, ...)        # a couple of final synchronization waits
madvise(..., MADV_DONTNEED)
exit(0)                                     # each worker thread, individually
...
exit_group(0)                               # the main thread, last
```

Every one of those primitives (`futex` WAKE/WAIT, `rt_sigprocmask`,
`gettid`, `madvise`, `exit`/`exit_group`) is already implemented here.
What's conspicuously *absent* from the KonjacOS guest trace, though, is
`mkdir`/`mkdirat` (real HotSpot creates `/tmp/hsperfdata_<user>/<pid>` --
a memory-mapped shared performance-counter file `jps`/`jstat` read --
early in startup, before any of the above): it never appears as an
"unimplemented syscall" line on the guest console at all, meaning
whatever code path real HotSpot takes to get to that `mkdir` either isn't
reached the same way here, or fails differently/earlier than on real
Linux (where a failed `mkdir` -- e.g. no writable `/tmp` -- is a
well-known, documented, non-fatal case: PerfData just gets disabled).
That divergence, not a single missing syscall, is the most concrete next
thing to chase: single-step (GDB, same harness `tools/trace_jvm.py`
already drives) through HotSpot's own `PerfMemory::create_memory_region`/
`os::Linux::create_file_for_heap`-equivalent path on the guest and
compare against the host trace's own timing, the same way items 36-38
tracked down the `strtold`/`abort` misdiagnosis. `-version`'s own required
output is real and confirmed; a clean process exit, and therefore a full
`java -jar` run to completion, is closer but not yet demonstrated.
