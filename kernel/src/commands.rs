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
    Command { name: "ls", summary: "[dir]  list a directory (default: the current one)", requires_apex: false, handler: cmd_ls },
    Command { name: "cd", summary: "[dir]  change directory (.., /, multi-level paths); no args prints cwd", requires_apex: false, handler: cmd_cd },
    Command { name: "pwd", summary: "print the current directory", requires_apex: false, handler: cmd_pwd },
    Command { name: "cat", summary: "<file>  print a file's contents", requires_apex: false, handler: cmd_cat },
    Command {
        name: "readat",
        summary: "<file> <offset> <len>  print a byte range without reading the whole file (tests vfs::read_file_range)",
        requires_apex: false,
        handler: cmd_readat,
    },
    Command { name: "write", summary: "<file> <text>  create/overwrite a file with <text>", requires_apex: false, handler: cmd_write },
    Command { name: "rm", summary: "<file>  delete a file", requires_apex: true, handler: cmd_rm },
    Command { name: "diskbench", summary: "time reads (whole files, 4 KiB pieces, random), writes and deletes on each disk (needs DOOM1.WAD; writes and deletes BENCH.TMP)", requires_apex: false, handler: cmd_diskbench },
    Command { name: "verify", summary: "[folder]  read every file under a folder and report any that fail", requires_apex: false, handler: cmd_verify },
    Command { name: "kfstest", summary: "[steps] [seed] [keep] [fill]  random writes, renames and deletes under /kfstest, checked as it goes", requires_apex: false, handler: cmd_kfstest },
    Command { name: "reboot", summary: "reset the machine", requires_apex: true, handler: cmd_reboot },
    Command { name: "halt", summary: "stop the CPU (interrupts off, spins on hlt)", requires_apex: true, handler: cmd_halt },
    Command { name: "apex", summary: "show whether an apex check is currently cached", requires_apex: false, handler: cmd_apex },
    Command { name: "ps", summary: "list running kernel threads (see also: multitasking)", requires_apex: false, handler: cmd_ps },
    Command { name: "kill", summary: "<id>  terminate a running task by ID (see `ps`)", requires_apex: false, handler: cmd_kill },
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

fn cmd_ls(rest: &str) {
    let listing = if rest.is_empty() { crate::vfs::list_current_dir() } else { crate::vfs::list_dir(rest) };
    match listing {
        Ok(mut entries) => {
            entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase())));
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
        println!("{}", crate::vfs::cwd_path_string());
        return;
    }
    if let Err(e) = crate::vfs::change_dir(rest) {
        println!("cd: {rest}: {e}");
    }
}

fn cmd_pwd(_rest: &str) {
    println!("{}", crate::vfs::cwd_path_string());
}

fn cmd_cat(rest: &str) {
    if rest.is_empty() {
        println!("usage: cat <file>");
        return;
    }
    match crate::vfs::read_file(rest) {
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
    match crate::vfs::read_file_range(path, offset, len) {
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

    match crate::vfs::write_file(name, &data) {
        Ok(()) => println!("wrote {} bytes to {name}", data.len()),
        Err(e) => println!("write: {name}: {e}"),
    }
}

fn cmd_rm(rest: &str) {
    if rest.is_empty() {
        println!("usage: rm <file>");
        return;
    }
    match crate::vfs::remove_file(rest) {
        Ok(()) => println!("removed {rest}"),
        Err(e) => println!("rm: {rest}: {e}"),
    }
}

/// Times the disks -- KonjacFS, then FAT16 if it's attached too: reading
/// DOOM1.WAD whole and in 4 KiB pieces, random 4 KiB reads, writing 1 MiB
/// and reading it back, listing a folder and deleting. Each test runs
/// [`BENCH_RUNS`] times and the median is shown, timed with the CPU's
/// cycle counter (calibrated against the timer first). KonjacFS's caches
/// are emptied before every run, so its numbers are first-time reads --
/// except "again", which repeats the random reads to show its cache.
fn cmd_diskbench(_rest: &str) {
    use crate::vfs;
    const BENCH_RUNS: usize = 5;
    let tsc = || unsafe { core::arch::x86_64::_rdtsc() };
    // Cycles per millisecond, over 20 timer ticks.
    let t0 = timer::ticks();
    while timer::ticks() == t0 {}
    let (c0, t1) = (tsc(), timer::ticks());
    while timer::ticks() < t1 + 20 {}
    let per_ms = ((tsc() - c0) * timer::HZ as u64 / 20 / 1000).max(1);
    let ops = || crate::block::stats().0;
    // Runs `prep` (untimed) then `f`, BENCH_RUNS times, and reports the
    // median time and the disk requests one run took.
    let run = |what: &str, bytes: usize, prep: &mut dyn FnMut(), f: &mut dyn FnMut() -> Result<(), &'static str>| {
        let mut times = [0u64; BENCH_RUNS];
        let mut requests = 0;
        for t in times.iter_mut() {
            prep();
            let (c, s) = (tsc(), ops());
            if let Err(e) = f() {
                println!("    {what:<26} {e}");
                return;
            }
            *t = (tsc() - c) * 1000 / per_ms;
            requests = ops() - s;
        }
        times.sort_unstable();
        let us = times[BENCH_RUNS / 2];
        let mut line = String::new();
        let _ = core::fmt::Write::write_fmt(&mut line, format_args!("    {what:<26} {:>4}.{} ms", us / 1000, us % 1000 / 100));
        if bytes > 0 && us > 0 {
            let _ = core::fmt::Write::write_fmt(&mut line, format_args!("  {:>5} MB/s", bytes as u64 / us));
        }
        let _ = core::fmt::Write::write_fmt(&mut line, format_args!("  ({requests} disk requests)"));
        println!("{line}");
    };
    let cold = &mut || crate::kfs::drop_caches();
    println!("diskbench ({}, median of {BENCH_RUNS} runs):", crate::block::backend_name());
    let data: Vec<u8> = (0..1024 * 1024u32).map(|i| (i * 7 + i / 4096) as u8).collect();
    // `/` first, then FAT16 at /fat if KonjacFS is `/`.
    let volumes = if crate::kfs::mounted() { alloc::vec![("KonjacFS", ""), ("FAT16", vfs::FAT_MOUNT)] } else { alloc::vec![("FAT16", "")] };
    for (name, root) in volumes {
        println!("  {name} ({}):", if root.is_empty() { "/" } else { root });
        let path = |p: &str| {
            let mut s = String::from(root);
            s.push('/');
            s.push_str(p);
            s
        };
        let wad = path("DOOM1.WAD");
        let size = vfs::stat_path(&wad).map(|(_, s)| s as usize).unwrap_or(0);
        let mut chunk = alloc::vec![0u8; 4096];
        run("read DOOM1.WAD", size, cold, &mut || vfs::read_file(&wad).map(|_| ()));
        run("read it in 4 KiB pieces", size, cold, &mut || {
            let mut f = vfs::open_file(&wad)?;
            let mut off = 0u64;
            while off < f.size as u64 {
                off += f.read_at(off, &mut chunk)?.max(1) as u64;
            }
            Ok(())
        });
        // The same 256 offsets each time.
        let random = |chunk: &mut [u8]| -> Result<(), &'static str> {
            let mut f = vfs::open_file(&wad)?;
            let mut x = 12345u64;
            for _ in 0..256 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                f.read_at((x >> 33) % (f.size as u64).saturating_sub(4096).max(1), chunk)?;
            }
            Ok(())
        };
        run("256 random 4 KiB reads", 256 * 4096, cold, &mut || random(&mut chunk));
        run("  again", 256 * 4096, &mut || {}, &mut || random(&mut chunk));
        let bench = path("BENCH.TMP");
        run("write 1 MiB", data.len(), &mut || { let _ = vfs::remove_file(&bench); }, &mut || vfs::write_file(&bench, &data));
        run("read it back", data.len(), cold, &mut || match vfs::read_file(&bench)? {
            d if d == data => Ok(()),
            _ => Err("** read-back mismatch: the disk returned different data **"),
        });
        let dir = if root.is_empty() { "/" } else { root };
        run("list the folder x20", 0, cold, &mut || {
            for _ in 0..20 {
                vfs::list_dir(dir)?;
            }
            Ok(())
        });
        run("delete it", 0, &mut || { let _ = vfs::write_file(&bench, &data); }, &mut || vfs::remove_file(&bench));
    }
}

/// Exercises KFS writing: random new files, overwrites, renames, moves,
/// new folders and deletes under /kfstest, checking every file
/// against what it should hold as it goes. `kfstest [steps] [seed] [keep]
/// [fill]`: `keep` leaves the folder there for `tools/kfs.py check`; `fill`
/// then fills the disk (see [`kfstest_fill`]).
fn cmd_kfstest(rest: &str) {
    use crate::vfs;
    if !crate::kfs::mounted() {
        println!("kfstest: no KFS disk attached");
        return;
    }
    let mut numbers = rest.split_whitespace().filter_map(|s| s.parse::<u64>().ok());
    let steps = numbers.next().unwrap_or(200) as u32;
    let seed = numbers.next().unwrap_or(timer::ticks() | 1);
    let keep = rest.split_whitespace().any(|s| s == "keep");
    let fill = rest.split_whitespace().any(|s| s == "fill");
    println!("kfstest: {steps} steps, seed {seed}");
    let mut state = seed.max(1);
    let mut rand = move |n: usize| -> usize {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    // Sizes either side of the inline limit and the extent size.
    const SIZES: [usize; 9] = [0, 1, 100, 2048, 2049, 5000, 65536, 70000, 300_000];
    // Files of 16 bytes or more start with "KFST", the tag and the size, so
    // tools/crash_test.py can check that any file it finds is whole.
    let content = |size: usize, tag: usize| -> Vec<u8> {
        let mut d: Vec<u8> = (0..size).map(|i| (i.wrapping_mul(31) + tag * 7 + i / 4096) as u8).collect();
        if size >= 16 {
            d[0..4].copy_from_slice(b"KFST");
            d[4..8].copy_from_slice(&(tag as u32).to_le_bytes());
            d[8..16].copy_from_slice(&(size as u64).to_le_bytes());
        }
        d
    };
    let join = |dir: &str, prefix: &str, n: u32| {
        let mut p = String::from(dir);
        p.push('/');
        p.push_str(prefix);
        p.push_str(&n.to_string());
        p
    };
    let under = |path: &str, dir: &str| path.strip_prefix(dir).is_some_and(|r| r.starts_with('/'));

    let base = "/kfstest";
    let _ = vfs::remove(base);
    if let Err(e) = vfs::create_dir(base) {
        println!("kfstest: can't create {base}: {e}");
        return;
    }
    let mut dirs: Vec<String> = alloc::vec![String::from(base)];
    let mut files: Vec<(String, usize, usize, bool)> = Vec::new(); // path, size, tag, changed since the last check
    let mut counter = 0u32;
    let start = timer::ticks();
    let (mut checked, mut bad) = (0usize, 0usize);
    for step in 0..steps {
        counter += 1;
        let op = rand(100);
        let result = if (op < 35 && files.len() < 120) || files.is_empty() {
            let path = join(&dirs[rand(dirs.len())], "f", counter);
            let (size, tag) = (SIZES[rand(SIZES.len())], rand(256));
            let r = vfs::write_file(&path, &content(size, tag));
            files.push((path, size, tag, true));
            r
        } else if op < 55 {
            let i = rand(files.len());
            let (size, tag) = (SIZES[rand(SIZES.len())], rand(256));
            files[i].1 = size;
            files[i].2 = tag;
            files[i].3 = true;
            vfs::write_file(&files[i].0, &content(size, tag))
        } else if op < 70 {
            let i = rand(files.len());
            let to = join(&dirs[rand(dirs.len())], "r", counter);
            let r = vfs::rename(&files[i].0, &to);
            files[i].0 = to;
            files[i].3 = true;
            r
        } else if op < 85 {
            let i = rand(files.len());
            vfs::remove_file(&files.swap_remove(i).0)
        } else if op < 95 || dirs.len() == 1 {
            let path = join(&dirs[rand(dirs.len())], "d", counter);
            let r = vfs::create_dir(&path);
            dirs.push(path);
            r
        } else {
            let dir = dirs[1 + rand(dirs.len() - 1)].clone();
            files.retain(|f| !under(&f.0, &dir));
            dirs.retain(|d| *d != dir && !under(d, &dir));
            vfs::remove(&dir)
        };
        if let Err(e) = result {
            println!("kfstest: step {step}: {e}");
            bad += 1;
            break;
        }
        if step % 250 == 249 {
            println!("kfstest: {} steps, {} ms, {} files, {} folders", step + 1, (timer::ticks() - start) * 1000 / timer::HZ as u64, files.len(), dirs.len());
        }
        // Every 25 steps, the files changed since the last check; at the
        // end, all of them.
        let last = step + 1 == steps;
        if step % 25 == 24 || last {
            for (path, size, tag, changed) in files.iter_mut().filter(|f| f.3 || last) {
                *changed = false;
                checked += 1;
                match vfs::read_file(path) {
                    Ok(d) if d == content(*size, *tag) => {}
                    Ok(d) => {
                        println!("kfstest: {path}: wrong contents ({} bytes, expected {size})", d.len());
                        bad += 1;
                    }
                    Err(e) => {
                        println!("kfstest: {path}: {e}");
                        bad += 1;
                    }
                }
            }
            for dir in &dirs {
                let expected = files.iter().filter(|f| f.0.rsplit_once('/').is_some_and(|(p, _)| p == dir)).count()
                    + dirs.iter().filter(|d| d.rsplit_once('/').is_some_and(|(p, _)| p == dir)).count();
                match vfs::list_dir(dir) {
                    Ok(v) if v.len() == expected => {}
                    Ok(v) => {
                        println!("kfstest: {dir}: {} entries, expected {expected}", v.len());
                        bad += 1;
                    }
                    Err(e) => {
                        println!("kfstest: {dir}: {e}");
                        bad += 1;
                    }
                }
            }
            if bad > 0 {
                break;
            }
        }
    }
    if fill && bad == 0 {
        bad += kfstest_fill(base);
    }
    // Names are case-sensitive.
    let (upper, lower) = (join(base, "Case", 0), join(base, "case", 0));
    let case_ok = vfs::write_file(&upper, b"upper").is_ok()
        && vfs::write_file(&lower, b"lower").is_ok()
        && vfs::read_file(&upper).is_ok_and(|d| d == b"upper")
        && vfs::read_file(&lower).is_ok_and(|d| d == b"lower");
    if !case_ok {
        println!("kfstest: Case0 and case0 aren't separate files");
        bad += 1;
    }
    let ms = (timer::ticks() - start) * 1000 / timer::HZ as u64;
    let cleanup = if keep { Ok(()) } else { vfs::remove(base) };
    println!(
        "kfstest: {} -- {checked} file checks, {} files and {} folders at the end, {ms} ms{}",
        if bad == 0 { "all good" } else { "FAILED" },
        files.len(),
        dirs.len(),
        if cleanup.is_ok() { "" } else { " (couldn't delete /kfstest)" }
    );
}

/// Writes 1 MiB files into `dir` until KFS reports the disk full, checks
/// the failed write left nothing behind and everything still reads, then
/// deletes them and checks the space came back. Returns the problems found.
fn kfstest_fill(dir: &str) -> usize {
    use crate::vfs;
    let free = || crate::kfs::info().map_or(0, |i| i.3);
    let name = |n: u32| {
        let mut p = String::from(dir);
        p.push_str("/fill");
        p.push_str(&n.to_string());
        p
    };
    let chunk: Vec<u8> = (0..1024 * 1024u32).map(|i| (i ^ (i >> 9)) as u8).collect();
    let before = free();
    let mut n = 0;
    let error = loop {
        match vfs::write_file(&name(n), &chunk) {
            Ok(()) => n += 1,
            Err(e) => break e,
        }
        if n > 1_000_000 {
            break "never filled up";
        }
    };
    let mut bad = 0;
    if vfs::stat_path(&name(n)).is_ok() {
        println!("kfstest: fill: the failed write left a file behind");
        bad += 1;
    }
    if n > 0 && !vfs::read_file(&name(n - 1)).is_ok_and(|d| d == chunk) {
        println!("kfstest: fill: the last file written doesn't read back");
        bad += 1;
    }
    let full = free();
    for k in 0..n {
        if let Err(e) = vfs::remove_file(&name(k)) {
            println!("kfstest: fill: deleting: {e}");
            bad += 1;
            break;
        }
    }
    let after = free();
    // The tree's own nodes may come out a block or two different.
    if after.abs_diff(before) > 8 {
        println!("kfstest: fill: {before} blocks free before, {after} after deleting");
        bad += 1;
    }
    println!("kfstest: fill: {n} MiB written, then \"{error}\" with {full} blocks left; {after} free after deleting (was {before})");
    bad
}

/// Reads every file under a folder (default `/`), so damage anywhere shows
/// up as an error naming the file. The summary also goes to the serial
/// port, for tools/bitrot_test.py. `verify [folder]`.
fn cmd_verify(rest: &str) {
    fn walk(dir: &str, depth: u32, files: &mut u32, bad: &mut u32, bytes: &mut u64) {
        let list = match crate::vfs::list_dir(dir) {
            Ok(l) => l,
            Err(e) => {
                println!("verify: {dir}: {e}");
                crate::sprintln!("verify: {dir}: {e}");
                *bad += 1;
                return;
            }
        };
        for e in list {
            let mut path = String::from(dir.trim_end_matches('/'));
            path.push('/');
            path.push_str(&e.name);
            if e.is_dir {
                if depth < 32 {
                    walk(&path, depth + 1, files, bad, bytes);
                }
                continue;
            }
            *files += 1;
            match crate::vfs::read_file(&path) {
                Ok(d) => *bytes += d.len() as u64,
                Err(err) => {
                    println!("verify: {path}: {err}");
                    crate::sprintln!("verify: {path}: {err}");
                    *bad += 1;
                }
            }
        }
    }
    let dir = if rest.trim().is_empty() { "/" } else { rest.trim() };
    let (mut files, mut bad, mut bytes) = (0, 0, 0);
    walk(dir, 0, &mut files, &mut bad, &mut bytes);
    println!("verify: {files} files, {} KiB read, {bad} errors", bytes / 1024);
    crate::sprintln!("verify: {files} files, {} KiB read, {bad} errors", bytes / 1024);
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
                // (Killing DOOM also closes its window: the desktop notices
                // the task is gone.)
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
    // `doom -iwad <path>`: DOOM on its own only looks for `doom1.wad`,
    // spelled that way, which a case-sensitive disk (KonjacFS) doesn't
    // match to `DOOM1.WAD`; `launch_doom` found the real name.
    let wad = DOOM_WAD.load(core::sync::atomic::Ordering::Acquire) as *const u8;
    let argv: [*const u8; 3] = [b"doom\0".as_ptr(), b"-iwad\0".as_ptr(), wad];
    unsafe {
        doomgeneric_Create(3, argv.as_ptr());
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

/// The WAD's path for DOOM's `-iwad`, set by `launch_doom`.
static DOOM_WAD: core::sync::atomic::AtomicPtr<u8> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

fn cmd_doom(_rest: &str) {
    match launch_doom() {
        Ok(id) => println!("doom: task #{id} spawned -- it opens in its own window. Close the window (or `kill {id}`) to stop it."),
        Err(e) => println!("doom: {e}"),
    }
}

/// Starts DOOM as its own task, unless it's already running -- shared by
/// the `doom` command and the desktop's taskbar.
pub fn launch_doom() -> Result<u64, &'static str> {
    if let Some(id) = crate::doom_driver::running_task() {
        return Ok(id);
    }
    let wad = crate::vfs::list_dir("/")
        .ok()
        .and_then(|l| l.into_iter().find(|e| !e.is_dir && e.name.eq_ignore_ascii_case("DOOM1.WAD")))
        .ok_or("DOOM1.WAD not found at the filesystem root (boot with the disk image attached)")?;
    // NUL-terminated for C, and alive for as long as DOOM might read its
    // argv: a few bytes per launch, never freed.
    let mut path = alloc::vec![b'/'];
    path.extend_from_slice(wad.name.as_bytes());
    path.push(0);
    DOOM_WAD.store(alloc::boxed::Box::leak(path.into_boxed_slice()).as_mut_ptr(), core::sync::atomic::Ordering::Release);
    // Drop whatever's still queued in the DOOM key ring, so DOOM's very
    // first DG_GetKey call doesn't see stale keystrokes as game input.
    crate::keyboard::clear_doom_events();
    let id = task::spawn_with_stack("doom", doom_task_entry, DOOM_STACK_SIZE).ok_or("failed to spawn task (out of task slots?)")?;
    crate::doom_driver::set_running_task(id);
    Ok(id)
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
/// list and becomes the path. Everything after the path, whitespace-
/// separated, becomes real `argv[1..]` for the spawned program (`argv[0]`
/// is the path itself) -- e.g. `run java -version` hands `java` a real
/// `argv` of `["java", "-version"]`, exactly like a real shell's `exec`
/// would. See item 41's own README entry for why real `envp` matters at
/// all: it's the concrete lever a real glibc/JVM needs for things like
/// `GLIBC_TUNABLES`/`JAVA_HOME`; real `argv[1..]` is the same kind of
/// lever for a program's own command-line flags.
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
    let mut parts = remaining.split_whitespace();
    let path = match parts.next() {
        Some(p) => p,
        None => {
            println!("usage: run [VAR=value ...] <file> [args...]  (flat binary, ELF64, or PE32+/.exe)");
            return;
        }
    };
    let mut argv: Vec<String> = Vec::with_capacity(1 + parts.clone().count());
    argv.push(path.to_string());
    argv.extend(parts.map(|s| s.to_string()));

    let bytes = match crate::vfs::read_file(path) {
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

    match crate::loader::load_and_run(name, &argv, &envp, &bytes) {
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
