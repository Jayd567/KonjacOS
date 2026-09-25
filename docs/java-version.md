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

## What isn't shown

The run does **not** reach a clean `exit_group`. After printing the
version banner, HotSpot goes on to spin up its own background threads
(compiler/GC/sweeper), each of which hits several syscalls this kernel
doesn't implement yet -- observed on screen: `273` (`set_robust_list`),
`334` (`rseq`), `302` (`prlimit64`), `96` (`gettimeofday`), `157`
(`prctl`), `229` (`clock_getres`), `107` (`geteuid`), `41` (`socket`).
Each returns this kernel's generic "unimplemented" response and HotSpot
evidently tolerates the failure and keeps going (no crash observed), but
across a 90s and a separate 100s run the console's final state was
identical and no shell prompt or process-exit indication ever reappeared
-- consistent with new threads continuing to start up (or spin) rather
than the VM shutting down cleanly. Real, unmodified JVM shutdown --
and therefore a real `java -jar` run all the way to completion -- needs at
least some of those syscalls implemented; not established or attempted
here. `-version`'s own required output is real and confirmed; a clean
process exit is not.
