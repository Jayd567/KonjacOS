//! The shell's command table: every builtin is one row here instead of a
//! branch in a hand-written match statement. `shell.rs` just splits the
//! typed line into a command word + rest-of-line, looks the word up in
//! [`COMMANDS`], and calls its handler -- adding a command means adding a
//! row, not touching dispatch logic. `requires_apex` is the hook a future
//! `apex` (privilege-escalation) command gate hangs off of: dispatch can
//! check that one field instead of every handler needing its own check.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::console;
use crate::port::outb;
use crate::task;
use crate::timer;
use crate::{print, println};

pub struct Command {
    pub name: &'static str,
    /// Shown by `help` next to the command name.
    pub summary: &'static str,
    /// Whether running this command should require apex (admin)
    /// privileges once that's implemented. Not enforced yet -- every
    /// command is `false` today -- but the field exists now so wiring an
    /// actual permission check later is a one-line change per command
    /// instead of a rewrite of dispatch.
    pub requires_apex: bool,
    pub handler: fn(&str),
}

pub static COMMANDS: &[Command] = &[
    Command { name: "help", summary: "show this list", requires_apex: false, handler: cmd_help },
    Command { name: "echo", summary: "<text>  print <text> back", requires_apex: false, handler: cmd_echo },
    Command { name: "clear", summary: "clear the screen", requires_apex: false, handler: cmd_clear },
    Command { name: "uptime", summary: "show how long the timer's been running", requires_apex: false, handler: cmd_uptime },
    Command { name: "meminfo", summary: "show physical/heap memory usage", requires_apex: false, handler: cmd_meminfo },
    Command {
        name: "alloc",
        summary: "<n>  heap-allocate a Vec of <n> u32s and sum it (default 16)",
        requires_apex: false,
        handler: cmd_alloc,
    },
    Command { name: "about", summary: "what is this", requires_apex: false, handler: cmd_about },
    Command { name: "ls", summary: "list the current directory", requires_apex: false, handler: cmd_ls },
    Command { name: "cd", summary: "[dir]  change directory (.., /, multi-level paths); no args prints cwd", requires_apex: false, handler: cmd_cd },
    Command { name: "pwd", summary: "print the current directory", requires_apex: false, handler: cmd_pwd },
    Command { name: "cat", summary: "<file>  print a file's contents", requires_apex: false, handler: cmd_cat },
    Command {
        name: "readat",
        summary: "<file> <offset> <len>  print a byte range without reading the whole file (tests fat16::read_file_range)",
        requires_apex: false,
        handler: cmd_readat,
    },
    Command { name: "write", summary: "<file> <text>  create/overwrite a file with <text>", requires_apex: false, handler: cmd_write },
    Command { name: "rm", summary: "<file>  delete a file", requires_apex: true, handler: cmd_rm },
    Command { name: "reboot", summary: "reset the machine", requires_apex: true, handler: cmd_reboot },
    Command { name: "halt", summary: "stop the CPU (interrupts off, spins on hlt)", requires_apex: true, handler: cmd_halt },
    Command { name: "apex", summary: "show whether an apex check is currently cached", requires_apex: false, handler: cmd_apex },
    Command { name: "ps", summary: "list running kernel threads (see also: multitasking)", requires_apex: false, handler: cmd_ps },
    Command { name: "kill", summary: "<id>  terminate a running task by ID (see `ps`)", requires_apex: false, handler: cmd_kill },
    Command { name: "gui", summary: "open the window manager (drag windows by their title bar; Esc to exit)", requires_apex: false, handler: cmd_gui },
    Command { name: "cdemo", summary: "run a small C function (malloc/free) compiled into the kernel -- DOOM groundwork", requires_apex: false, handler: cmd_cdemo },
    Command { name: "cio", summary: "run compiled C printf + file I/O (fopen/fread/fwrite) -- DOOM groundwork", requires_apex: false, handler: cmd_cio },
    Command { name: "doom", summary: "launch DOOM (needs DOOM1.WAD at the filesystem root)", requires_apex: false, handler: cmd_doom },
    Command {
        name: "run",
        summary: "<file>  load and run a flat binary, ELF64, or PE32+ (.exe) executable as its own task",
        requires_apex: false,
        handler: cmd_run,
    },
];

/// Looks up `name` in [`COMMANDS`]. `shell.rs` owns what happens on a hit
/// vs. a miss (including, eventually, the apex prompt) -- this is just
/// the table lookup.
pub fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.name == name)
}

fn cmd_help(_rest: &str) {
    println!("commands:");
    for cmd in COMMANDS {
        let apex_tag = if cmd.requires_apex { " [apex]" } else { "" };
        println!("  {:<12} {}{}", cmd.name, cmd.summary, apex_tag);
    }
}

fn cmd_echo(rest: &str) {
    println!("{rest}");
}

fn cmd_clear(_rest: &str) {
    console::CONSOLE.lock().clear();
}

fn cmd_uptime(_rest: &str) {
    let secs = timer::uptime_seconds();
    println!(
        "up {}h {}m {}s ({} ticks @ {}Hz)",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60,
        timer::ticks(),
        timer::HZ
    );
}

fn cmd_meminfo(_rest: &str) {
    crate::memory::print_info();
}

fn cmd_alloc(rest: &str) {
    let n: usize = rest.trim().parse().unwrap_or(16);
    let mut v: Vec<u32> = Vec::new();
    for i in 0..n as u32 {
        v.push(i);
    }
    let sum: u64 = v.iter().map(|&x| x as u64).sum();
    println!("allocated a Vec<u32> of {} elements (heap-backed), sum = {}", v.len(), sum);
    drop(v);
    println!("dropped it -- freed back to the heap.");
}

fn cmd_about(_rest: &str) {
    println!("KonjacOS -- a hobby kernel, still very much under construction.");
}

fn cmd_ls(_rest: &str) {
    match crate::fat16::list_current_dir() {
        Ok(entries) => {
            if entries.is_empty() {
                println!("(empty)");
            }
            for entry in &entries {
                if entry.is_dir {
                    println!("  <DIR>  {}", entry.name);
                } else {
                    println!("  {:>8}  {}", entry.size, entry.name);
                }
            }
        }
        Err(e) => println!("ls: {e}"),
    }
}

fn cmd_cd(rest: &str) {
    // Windows' `cd` with no arguments just prints the current directory
    // instead of navigating -- matching that instead of the Unix
    // behaviour (go to a "home" directory) since there's no such notion
    // here anyway.
    if rest.is_empty() {
        println!("{}", crate::fat16::cwd_path_string());
        return;
    }
    if let Err(e) = crate::fat16::change_dir(rest) {
        println!("cd: {rest}: {e}");
    }
}

fn cmd_pwd(_rest: &str) {
    println!("{}", crate::fat16::cwd_path_string());
}

fn cmd_cat(rest: &str) {
    if rest.is_empty() {
        println!("usage: cat <file>");
        return;
    }
    match crate::fat16::read_file(rest) {
        Ok(data) => match core::str::from_utf8(&data) {
            Ok(text) => print!("{text}"),
            Err(_) => println!("cat: {rest} is not valid UTF-8 ({} bytes)", data.len()),
        },
        Err(e) => println!("cat: {rest}: {e}"),
    }
}

/// Exercises `fat16::read_file_range` directly from the shell -- a small,
/// deliberate way to prove the partial-read path actually returns the
/// right bytes (not just "compiles"), ahead of anything else (a real
/// `pread` syscall, demand-paged mmap) building on top of it.
fn cmd_readat(rest: &str) {
    let mut parts = rest.split_whitespace();
    let (Some(path), Some(offset_str), Some(len_str)) = (parts.next(), parts.next(), parts.next()) else {
        println!("usage: readat <file> <offset> <len>");
        return;
    };
    let Ok(offset) = offset_str.parse::<u32>() else {
        println!("readat: {offset_str} is not a valid offset");
        return;
    };
    let Ok(len) = len_str.parse::<usize>() else {
        println!("readat: {len_str} is not a valid length");
        return;
    };
    match crate::fat16::read_file_range(path, offset, len) {
        Ok(data) => match core::str::from_utf8(&data) {
            Ok(text) => println!("{} bytes: {text:?}", data.len()),
            Err(_) => println!("{} bytes (not valid UTF-8)", data.len()),
        },
        Err(e) => println!("readat: {path}: {e}"),
    }
}

fn cmd_write(rest: &str) {
    let (name, text) = match rest.split_once(' ') {
        Some((n, t)) => (n, t.trim_start()),
        None => (rest, ""),
    };
    if name.is_empty() {
        println!("usage: write <file> <text>");
        return;
    }
    // A trailing newline makes the common case (a short note) read back
    // nicely with `cat` -- matching what typing a line into most text
    // editors and saving would leave you with.
    let mut data: Vec<u8> = Vec::new();
    data.extend_from_slice(text.as_bytes());
    data.push(b'\n');

    match crate::fat16::write_file(name, &data) {
        Ok(()) => println!("wrote {} bytes to {name}", data.len()),
        Err(e) => println!("write: {name}: {e}"),
    }
}

fn cmd_rm(rest: &str) {
    if rest.is_empty() {
        println!("usage: rm <file>");
        return;
    }
    match crate::fat16::remove_file(rest) {
        Ok(()) => println!("removed {rest}"),
        Err(e) => println!("rm: {rest}: {e}"),
    }
}

fn cmd_apex(_rest: &str) {
    if crate::apex::is_authenticated() {
        println!("apex: authenticated (cached -- won't re-prompt for a few minutes)");
    } else {
        println!("apex: not authenticated -- the next [apex] command will prompt");
    }
}

/// Terminates a task by ID (`task::kill`) rather than waiting for it to
/// exit on its own -- the shell-visible complement to `exit`/`abort` now
/// actually freeing a task's slot (see `task.rs`'s `kill`/`exit_current`
/// and `schedule`'s reaping sweep). Task 0 is always the shell itself
/// (`task::init` assigns it first, before anything else can spawn and grab
/// a lower ID), so refusing to kill it isn't a special case worth plumbing
/// through `task.rs` -- just don't hand it a self-destructive ID here.
fn cmd_kill(rest: &str) {
    let rest = rest.trim();
    if rest.is_empty() {
        println!("usage: kill <id>  (see `ps` for IDs)");
        return;
    }
    let id: u64 = match rest.parse() {
        Ok(v) => v,
        Err(_) => {
            println!("kill: {rest}: not a valid task ID");
            return;
        }
    };
    if id == 0 {
        println!("kill: refusing to kill task 0 (the shell itself)");
        return;
    }

    let name = task::list().into_iter().find(|&(tid, ..)| tid == id).map(|(_, name, ..)| name);
    match name {
        None => println!("kill: no live task with ID {id}"),
        Some(name) => {
            if task::kill(id) {
                if name == "doom" {
                    // DOOM owns the framebuffer for as long as it's
                    // running -- its last rendered frame is still sitting
                    // there even though the task drawing it is now gone,
                    // so wipe it back to a clean console instead of
                    // leaving a frozen game screen with nothing left
                    // updating it.
                    console::CONSOLE.lock().clear();
                }
                println!("kill: task #{id} ({name}) terminated");
            } else {
                println!("kill: task #{id} ({name}) was already terminated");
            }
        }
    }
}

fn cmd_ps(_rest: &str) {
    println!("  {:<4} {:<10} {:<10} {:>10}", "ID", "NAME", "STATE", "TICKS");
    for (id, name, state, ticks_run, is_current) in task::list() {
        let marker = if is_current { "*" } else { " " };
        println!("{marker} {:<4} {:<10} {:<10} {:>10}", id, name, state.label(), ticks_run);
    }
    let (a, b) = demo_counters();
    println!("counter-a's loop count: {a}, counter-b's loop count: {b} (proof both are actually spinning, not just scheduled)");
}

// --- Demo kernel threads --------------------------------------------------
//
// Two background tasks spawned at boot purely to prove the scheduler in
// task.rs actually preempts and round-robins rather than just compiling:
// each spins bumping its own counter, so `ps`'s TICKS column (how many
// timer ticks task.rs actually handed each task) climbing for *all three*
// tasks -- these two plus the shell -- at once is the visible evidence that
// this is real preemptive multitasking, not just one thread pretending.

static DEMO_COUNTER_A: AtomicU64 = AtomicU64::new(0);
static DEMO_COUNTER_B: AtomicU64 = AtomicU64::new(0);

pub fn spawn_demo_tasks() {
    task::spawn("counter-a", demo_task_a);
    task::spawn("counter-b", demo_task_b);
}

fn demo_task_a() {
    loop {
        DEMO_COUNTER_A.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
}

fn demo_task_b() {
    loop {
        DEMO_COUNTER_B.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
}

fn demo_counters() -> (u64, u64) {
    (DEMO_COUNTER_A.load(Ordering::Relaxed), DEMO_COUNTER_B.load(Ordering::Relaxed))
}

fn cmd_gui(_rest: &str) {
    crate::wm::run();
}

// --- C toolchain demo (DOOM-porting groundwork) ---------------------------
//
// `cdemo_run` is real, compiled C code (`csrc/cdemo.c`, built by
// `build.rs` and linked straight into this binary) that heap-allocates an
// array via `libc_shim.rs`'s `malloc`, fills and sums it, frees it, and
// reports the sum back here via `konjac_report`. Proving this whole round
// trip works -- C compiled, linked, calling into Rust-backed libc, Rust
// receiving a callback from it -- is the actual point: it's the pipeline a
// real C codebase like doomgeneric would need, exercised end to end before
// ever pulling that codebase in.

unsafe extern "C" {
    fn cdemo_run() -> i64;
}

/// Called *from* the C demo (see `csrc/cdemo.c`) to report its computed
/// result back to Rust, so the C side doesn't need to know anything about
/// KonjacOS's console.
#[unsafe(no_mangle)]
pub extern "C" fn konjac_report(value: i64) {
    println!("cdemo: C code (via malloc/free through the kernel heap) computed sum = {value}");
}

fn cmd_cdemo(_rest: &str) {
    println!("cdemo: calling into compiled C code...");
    let result = unsafe { cdemo_run() };
    println!("cdemo: cdemo_run() returned {result} (expected 4032 = sum of 0,2,4,...,126)");
}

// `iodemo_run` (csrc/iodemo.c) exercises printf/fprintf and a real
// fopen/fwrite/fread/fclose round trip through fat16.rs (via cfile.rs),
// the other half of the DOOM-porting groundwork's libc shim.
unsafe extern "C" {
    fn iodemo_run() -> i32;
}

fn cmd_cio(_rest: &str) {
    let result = unsafe { iodemo_run() };
    match result {
        0 => println!("cio: iodemo_run() returned 0 (success)"),
        code => println!("cio: iodemo_run() failed, step {code}"),
    }
}

// --- DOOM --------------------------------------------------------------
//
// `doomgeneric_Create`/`doomgeneric_Tick` are doomgeneric's own entry
// points (`doomgeneric.h`); `doomgeneric_Create` runs DOOM's startup
// (WAD loading, `DG_Init`, etc.) and `doomgeneric_Tick` runs one frame's
// worth of game logic + rendering, expected to be called in a loop by the
// platform driver's own `main` (here, `doom_task_entry` below, instead of
// a real `main` -- see `csrc/doom/doomgeneric_konjac.c`'s doc comment).
unsafe extern "C" {
    fn doomgeneric_Create(argc: i32, argv: *const *const u8);
    fn doomgeneric_Tick();
}

/// Runs as its own kernel task (see `cmd_doom` below) rather than
/// straight in the shell's own call stack: DOOM's engine expects a large,
/// dedicated stack (`p_mobj.c`/`r_bsp.c` recursion, big on-stack
/// structures), and running its main loop forever would otherwise block
/// the shell -- and everything else -- from ever getting scheduled again.
fn doom_task_entry() {
    // A single dummy argv[0] rather than a real argc/argv pair: nothing
    // on DOOM's startup path here actually parses command-line options
    // (DOOM1.WAD is found via D_FindWADByName's bare-filename check in
    // d_iwad.c, not an -iwad argument -- it just needs to be sitting at
    // the filesystem root, true as long as DOOM is launched from cwd `/`,
    // which every task is today since there's no per-task cwd yet, just
    // fat16.rs's single global one), but keeping argv[0] non-null avoids
    // a latent null-pointer footgun in anything that ever calls
    // M_GetExecutableName (myargv[0]) even though nothing on this path
    // does today.
    let argv0: *const u8 = b"doom\0".as_ptr();
    let argv: [*const u8; 1] = [argv0];
    unsafe {
        doomgeneric_Create(1, argv.as_ptr());
        crate::doom_driver::doom_window_init();
        loop {
            doomgeneric_Tick();
        }
    }
}

/// DOOM's own recursive renderer and big static engine tables want more
/// than the kernel's default per-task stack; this mirrors the `cdemo`/
/// `cio` C-toolchain groundwork's use of `spawn_with_stack` for anything
/// that isn't a tiny leaf demo.
const DOOM_STACK_SIZE: usize = 1024 * 1024;

fn cmd_doom(_rest: &str) {
    println!("doom: launching as its own task -- quit from DOOM's own menu to return to the shell, or run `kill <id>` (see `ps`) to stop it early.");
    // Drop whatever's still queued from typing "doom" + Enter itself,
    // so DOOM's very first DG_GetKey call doesn't see them as game
    // input (see clear_doom_events's own doc comment).
    crate::keyboard::clear_doom_events();
    match task::spawn_with_stack("doom", doom_task_entry, DOOM_STACK_SIZE) {
        Some(id) => println!("doom: task #{id} spawned"),
        None => println!("doom: failed to spawn task (out of task slots?)"),
    }
}

// --- The "big trio" loader ------------------------------------------------
//
// `run` hands a file's raw bytes to `loader.rs`, which sniffs the magic
// bytes and figures out for itself whether it's looking at a flat binary, an
// ELF64 executable, or a PE32+ (.exe) executable -- see that module's doc
// comment for how one kernel ends up able to load all three.

/// `run [VAR=value ...] <file>` -- real Linux `env`-style leading
/// assignments, consumed here rather than by a separate shell built-in
/// since this is the only place KonjacOS ever hands a real Linux program
/// its environment. Each leading whitespace-separated token containing an
/// `=` (with something before it, so a path that happens to start with
/// `=` -- vanishingly unlikely, but not this parser's business to
/// misinterpret) is taken as `NAME=value` and added to the child's real
/// `envp`; the first token that isn't shaped like an assignment ends the
/// list and becomes the path. No real argv[1..] support yet -- see
/// `loader.rs`'s own `argv0` doc comment for that still-open gap -- this
/// is specifically about environment variables, the first real one this
/// kernel has ever handed a spawned program (see item 41's own README
/// entry for why real `envp` matters at all: it's the concrete lever a
/// real glibc/JVM needs for things like `GLIBC_TUNABLES`/`JAVA_HOME`).
fn cmd_run(rest: &str) {
    let mut remaining = rest.trim();
    let mut envp: Vec<String> = Vec::new();
    while let Some((token, after)) = remaining.split_once(' ') {
        let after = after.trim_start();
        match token.split_once('=') {
            Some((name, _)) if !name.is_empty() => {
                envp.push(token.to_string());
                remaining = after;
            }
            _ => break,
        }
    }
    let path = remaining;
    if path.is_empty() {
        println!("usage: run [VAR=value ...] <file>  (flat binary, ELF64, or PE32+/.exe)");
        return;
    }
    let bytes = match crate::fat16::read_file(path) {
        Ok(data) => data,
        Err(e) => {
            println!("run: {path}: {e}");
            return;
        }
    };

    // task::spawn_user needs a `&'static str` name -- it outlives the
    // shell's own line buffer, which `path` is borrowed from -- so this
    // uses a small fixed name per *detected format* instead of leaking
    // memory to hand it the real filename; `ps` cares about telling tasks
    // apart, not about reproducing exact filenames.
    let name = match crate::loader::detect(&bytes) {
        crate::loader::Format::Elf => "run:elf",
        crate::loader::Format::Pe => "run:pe",
        crate::loader::Format::Flat => "run:bin",
    };

    match crate::loader::load_and_run(name, path, &envp, &bytes) {
        Ok((id, format)) => println!("run: {path}: recognized as {}, task #{id} spawned", format.label()),
        Err(e) => println!("run: {path}: {e}"),
    }
}

fn cmd_reboot(_rest: &str) {
    println!("rebooting...");
    reboot();
}

fn cmd_halt(_rest: &str) {
    println!("halting.");
    unsafe {
        core::arch::asm!("cli");
    }
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

/// Resets the machine via the classic keyboard-controller reset line
/// (pulse the "reset" bit on the 8042's command port). This is the same
/// trick real BIOSes/bootloaders use for a software reboot on hardware
/// with no ACPI reset register wired up; QEMU honours it too.
fn reboot() -> ! {
    unsafe {
        // Flush any queued keyboard-controller command first.
        while crate::port::inb(0x64) & 0x02 != 0 {}
        outb(0x64, 0xFE);
        // If the controller didn't reset us (shouldn't happen), just halt.
        core::arch::asm!("cli");
    }
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
