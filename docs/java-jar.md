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

**Not yet root-caused.** The next concrete step is finding what actually
*prints* `Error: An unexpected error occurred while trying to open file`
in the real OpenJDK source (a `LauncherHelper`/native-launcher string
search against the real JDK sources would locate it directly, rather
than guessing) to learn what condition really triggers it and whether
it's meant to be fatal -- then, separately, adding `write` (syscall 1)
to the same targeted-breakpoint trace to see whether *this exact
message* is what's being printed early, or something else, and whether
any further real output ever follows it.
