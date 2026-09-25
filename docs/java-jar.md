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

## The real remaining puzzle: stuck, not slow

A follow-up trace watched for both `write` *and* `writev` (syscall 20 --
the path musl/glibc stdio actually flushes through in practice, per this
module's own docs) directly, logging every single call to either
regardless of content. Across a full traced run, **neither is ever
called even once.** Combined with a quiet (untraced) run showing the
exact same unchanging screen for a full 10 minutes: the `Error:` line
was the *last* thing this process ever wrote to any output stream in
every run tried, traced or not. The earlier framing ("a quiet run stalls
but a traced run keeps progressing") was based on incomplete evidence --
what the GDB trace actually shows is real syscall activity (genuine
`pread64`s into `lib/modules`, reaching the same steady-state futex loop
a successful `-version` run also passes through) *without* it ever
translating into new console output, in either run. That's consistent
with one simpler explanation: this specific run is genuinely stuck --
retrying something (most plausibly, class resolution failing and being
attempted again through JDK's module/classpath search machinery) rather
than making forward progress toward printing this program's own
`System.out.println` line, and no timing-sensitive race or output-path
gap is needed to explain the earlier observations.

Checked directly against the existing trace data (free -- no new guest
run needed): the `pread64` offsets into `lib/modules` are **not** a
retry loop. 33 calls, 32 distinct offsets, spread from small values up
past `0x1bd99e9` (~29 MB into the archive) -- real, varied reads
consistent with genuinely loading many different classes out of the
modules image, not the same lookup failing and retrying. That reframes
the puzzle: this *is* real forward progress, just real progress that
produces no console output at all until whatever eventually needs
`PrintStream`/`System.out` (or the launcher's own completion) is
reached -- meaning a quiet run's unchanging screen for 10 minutes isn't
necessarily evidence of being stuck, only evidence that this specific
window wasn't long enough. `-jar` needs strictly more class loading than
`-version` ever does (real JVM bootstrap classes *and* this program's
own `Hello` class, where `-version` never resolves a single application
class) -- a proportionally longer real time to reach any output at all,
on top of `-version`'s own already-slow real path to `exit_group`, is a
real, plausible, unexcluded explanation on its own.

A quiet run given a full **30 minutes** of real wall-clock time -- three
times `-version`'s entire real-time path to a clean `exit_group`, and
its own longest attempt by far -- still shows the exact same unchanging
screen. That rules "just needs more time" out conclusively: this genuinely
is stuck, not slow.

**Not root-caused for `-jar` specifically, but isolated.** What was
established for `-jar`: real, varied, non-repeating reads happen (ruling
out the simplest "tight retry loop" shape); nothing is ever written to
console or the process's own output streams after the initial `Error:`
line; thirty real minutes produces no further change (ruling out "just
needs more time"). What wasn't established is *where* forward progress
actually stops for good, only that it does.

## `java -cp` (no jar/zip layer at all): works completely

The other concrete lever this document named -- a `java -cp` run
against `Hello.class` copied directly onto the disk (no `jar`/zip
packaging step, `javac`'s own output copied as-is) -- isolates the
question cleanly: is this a jar/zip-reading problem specifically, or a
class-resolution/execution problem generally?

```
konjac> run /usr/lib/jvm/java-21-openjdk-amd64/bin/java -cp / Hello world
...
Hello from a real java -jar run on KonjacOS!
arg: world
```

**It's the jar/zip-reading path specifically.** `java -cp / Hello world`
runs to completion in a single quiet attempt -- no `Error:` line at all,
real class loading, real bytecode execution, this program's own real
`System.out.println` output, and its real `argv[1]` (`"world"`) printed
correctly. This is a genuinely new milestone in its own right: the
*first* real, user-written Java program (as opposed to the JDK's own
`java -version` banner) to run to completion on KonjacOS.

This narrows `-jar`'s remaining problem precisely: something specific to
opening a class file *through* a jar/zip archive (central directory
parsing, `ZipFile`'s own native code, or the exact `readAttributes` call
JDK-8313765 already implicated) is where forward progress stops --
general class loading, bytecode execution, and program I/O are all
confirmed working. The next concrete step for `-jar` itself is
narrower than before: focus specifically on `java.util.zip`/
`jdk.internal.loader.URLClassPath`'s jar-reading native code path, not
class loading in general.
