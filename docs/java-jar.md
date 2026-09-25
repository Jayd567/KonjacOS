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

## Still not resolved

With `statx` implemented, `run java -jar /hello.jar world` no longer
shows any "unimplemented syscall" line at all -- but still fails with
the exact same `Error: An unexpected error occurred while trying to open
file /hello.jar`. Whatever's actually failing now returns a value this
kernel considers valid but Java doesn't, rather than hitting the
catch-all. A targeted GDB trace (open/openat/fstat/statx/pread64/mmap,
filtered the same low-overhead way `docs/java-version.md`'s own `mkdir`
investigation used) is the obvious next step -- not yet run to
completion as of this note.
