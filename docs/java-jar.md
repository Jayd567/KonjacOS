# Toward `java -jar`: the first new blocker, `statx`

With `java -version` reaching a real, clean `exit_group` (see
`docs/java-version.md`), the natural next real test is a `java -jar` run
that actually loads and executes bytecode, not just prints a banner and
exits -- the real next milestone on the way to a `java -jar
minecraft_server.jar`-shaped goal.

## Building a real test jar

A minimal `Hello.java` (`System.out.println`, echoes its `argv`),
compiled and packaged with this same machine's own `javac`/`jar` --
real, unmodified host tools, same as every other artifact this project's
JVM work has staged onto the guest disk -- copied onto `disk.img` at
`/hello.jar`.

## The blocker: `statx`

`run /usr/lib/jvm/java-21-openjdk-amd64/bin/java -jar /hello.jar world`
reaches the shell, spawns the task, and immediately fails:

```
Error: An unexpected error occurred while trying to open file /hello.jar
```

with `linux_syscall: unimplemented syscall number 332` on screen just
before it. `332` is `statx` -- unlike `java -version` (which never opens
a user-supplied file by path), `java -jar` needs to stat the jar itself
before opening it as a zip, and glibc's own zip/file-checking code calls
`statx` directly rather than falling back to `newfstatat`, even though
both exist. This kernel had nothing to answer it with at all.

Implemented in `linux_syscall.rs`: `sys_statx`, filling in the same
honest subset of `struct statx` (256 bytes, real x86_64 layout)
`write_stat` already gives plain `stat`/`fstat` -- type, mode, nlink,
ino, size -- and reporting `stx_mask` to match exactly those bits, so a
caller asking for fields this kernel doesn't track (`STATX_BTIME`, ...)
honestly sees them absent rather than a fabricated value. `dirfd`/
`flags`/`mask` are read but not consulted, same precedent `sys_newfstatat`
already set.

Verified: `run hello.exe` unaffected.

## The `Error` message is real, but not obviously fatal

With `statx` implemented, `run java -jar /hello.jar world` no longer
shows any "unimplemented syscall" line -- but still prints the exact
same `Error: An unexpected error occurred while trying to open file
/hello.jar`, early, consistently, every run.

A targeted GDB trace (filtered `linux_syscall_handler` entry breakpoints,
the same low-overhead technique the `mkdir`/`hsperfdata` investigation in
`docs/java-version.md` used, watching `open`/`openat`/`fstat`/`statx`/
`pread64`/`read`/`lseek`/`mmap`) shows the *real* `statx("/hello.jar")`
call succeeding (`ret=0`), then a real `openat("/hello.jar")` succeeding
(`ret=3`), then a sequence of `lseek`+`read` pairs against that fd that
match real ZIP parsing exactly: `lseek` to `file_size - 22` (the real
byte offset a ZIP End-Of-Central-Directory record search starts from),
short reads of the sizes real EOCD/central-directory/local-header
structures actually have, all succeeding with plausible byte counts.
Execution continues well past this -- `libjimage.so` opens, and dozens
of `pread64` calls follow with large, varying offsets (tens of megabytes
in, consistent with real class-loading reads out of the real 140 MB
`lib/modules` jimage archive) -- with nothing in the trace looking like
an error return.

That's the puzzle: the `Error:` text is real Java-level output (not a
kernel panic, not this kernel inventing it), but the process visibly
keeps doing real, varied I/O well after printing it, which real, fatal
launcher errors don't do -- a fatal `Error:` from `LauncherHelper`
normally means an immediate `System.exit`. Two explanations were tested:

* **"It just needs more wall-clock time."** Ruled out: a quiet
  (untraced) run given a full 10 minutes of real time shows the exact
  same, unchanging screen the entire time -- the `Error:` line and
  nothing else, ever again visible on screen. If the deep `pread64`
  activity the GDB trace observed were genuine forward progress toward
  printing this program's own `System.out.println` output, ten real
  minutes without emulation-tracing overhead should have been more than
  enough (`java -version`'s own banner appears within roughly a minute
  of quiet real time).
* Still open: whether that deep, varied-offset I/O activity the GDB
  trace observed is genuine (if slow) forward progress that a quiet run
  simply never gets to print anything for before this session's
  observation windows ended, or a real loop (e.g. a repeated class-load-
  failure/retry cycle) that never terminates. Not established.

## What the message actually is: a real, known JDK issue -- but not fatal here

A web search (this machine's own real OpenJDK `src.zip` turned out to be
a broken symlink -- one of the two dangling ones item 34's own disk
staging already knew about -- so this used real, external ground truth
instead) identifies the exact real-world bug: **JDK-8313765**, introduced
by `openjdk/jdk21u@4cf572e`, changed how `UnixFileAttributeViews$Basic
.readAttributes()` reads file metadata during `ZipFile`/jar-opening --
and on some real, physical Linux systems (documented reports: Termux/
Android, some containers, at least one real Samsung phone -- genuinely
device/kernel-dependent, "unknown why only some devices are affected")
a `FileSystemException` wrapping `"Function not implemented"` (the exact
`strerror` text for a real `ENOSYS`) comes back from that read and
`LauncherHelper` reports exactly this "unexpected error" message. A
documented workaround flag,
`-Djdk.util.zip.disableZip64ExtraFieldValidation=true`, exists for a
related report -- tried directly against the guest here and made no
difference, so that specific flag's code path isn't the one being hit,
but the underlying "some real syscall this attribute read needs returns
ENOSYS" diagnosis fits this kernel exactly: `linux_syscall.rs` returns a
real `ENOSYS` for several things by design (`rseq`, `getrandom` when
`RDRAND` isn't available, `prlimit64` when actually *setting* a new
limit rather than querying one -- all three genuinely fire during this
exact run, confirmed by a targeted trace logging every negative-return
syscall).

**Concretely, though, this is not fatal here.** A GDB trace watching for
`write`/`writev` and any negative return continues *well past* the
`Error:` line -- deep into real class loading (`libjimage.so`, real
`pread64`s into the 140 MB `lib/modules` archive), and eventually
reaches the *exact same* steady-state safepoint-polling futex loop
(`FUTEX_WAIT_BITSET`, `ETIMEDOUT`, repeating) that a real `java -version`
run reaches before its own eventual clean `exit_group` (see
`docs/java-version.md`). This strongly suggests the `Error:` line is a
real but non-fatal warning from one specific attribute-read call (most
likely the launcher's own CDS-archive-related jar check, which has its
own independent, tolerant error handling separate from the main
classloading path), not something that stops the JVM.

## The real remaining puzzle: quiet runs don't show the same progress

A *quiet* (untraced) run given a full 10 minutes of real wall-clock time
shows the exact same, unchanging screen the entire time -- no further
output, ever. That's the opposite of what the GDB-traced run's continued
deep activity would predict: if that activity were genuine progress
happening at any real, reasonable pace, ten real minutes with no
tracing overhead should easily be enough (`java -version`'s own banner
appears within roughly a minute of quiet real time, and it reaches full
`exit_group` well within this session's own trace windows).

Two explanations remain open, not yet distinguished:

* The GDB-breakpoint-slowed execution's altered timing avoids a real,
  timing-sensitive bug (a missed wakeup, a race in this kernel's own
  single-CPU scheduler or futex-wake path) that an unthrottled quiet run
  hits reliably -- i.e. tracing accidentally "fixes" a real race by
  slowing everything down enough to avoid it.
* Something about console output itself is the actual gap (a `writev`
  buffering/flush difference for this specific, heavier call pattern)
  rather than JVM progress -- i.e. the process really is still working
  underneath, but nothing further ever reaches the visible framebuffer
  console in a quiet run specifically.

**Not yet root-caused.** The next concrete step is watching `writev`
(syscall 20, not just plain `write`) in the same targeted trace, since
this module's own docs already note musl/glibc stdio flushes through
`writev`, not `write`, in practice -- the previous trace only watched
`write` and caught nothing, which is consistent with output never using
that path here at all rather than with no output happening. Separately,
comparing a quiet run's and a traced run's serial console log (not just
the framebuffer) might catch output the framebuffer screendump missed
entirely.
