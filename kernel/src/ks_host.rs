//! The kernel side of ks (KonjacShell): the [`ks::Host`] that gives the
//! language files, the clock and the screen, the commands only the kernel
//! can answer (`ps`, `mem`, `disks`, ...), and the original commands from
//! `commands.rs`, which run as text commands. See docs/ks-design.md.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use ks::{Call, Ctx, Engine, FileInfo, Record, Runner, Shape, ShellError, Signature, Value};

use crate::{commands, console, keyboard, print, println, task, timer, vfs};

pub struct KernelHost;

fn info(e: &vfs::DirEntry) -> FileInfo {
    FileInfo { name: e.name.clone(), is_dir: e.is_dir, size: e.size, modified: e.modified, created: e.created }
}

fn err(e: &'static str) -> String {
    e.to_string()
}

impl ks::Host for KernelHost {
    fn print(&mut self, text: &str) {
        print!("{text}");
    }

    fn cwd(&mut self) -> String {
        vfs::cwd_path_string()
    }

    fn set_cwd(&mut self, path: &str) -> Result<(), String> {
        vfs::change_dir(path).map_err(err)
    }

    fn absolute(&mut self, path: &str) -> String {
        vfs::absolute(path)
    }

    fn list_dir(&mut self, path: &str) -> Result<Vec<FileInfo>, String> {
        Ok(vfs::list_dir(path).map_err(err)?.iter().map(info).collect())
    }

    fn stat(&mut self, path: &str) -> Result<FileInfo, String> {
        // The times are in the folder listing, so look the name up there.
        let abs = vfs::absolute(path);
        if let Some((dir, name)) = abs.rsplit_once('/') {
            let dir = if dir.is_empty() { "/" } else { dir };
            if let Ok(list) = vfs::list_dir(dir) {
                let found = list.iter().find(|e| e.name == name).or_else(|| list.iter().find(|e| e.name.eq_ignore_ascii_case(name)));
                if let Some(e) = found {
                    return Ok(info(e));
                }
            }
        }
        let (is_dir, size) = vfs::stat_path(&abs).map_err(err)?;
        let name = abs.rsplit('/').next().unwrap_or("").to_string();
        Ok(FileInfo { name, is_dir, size, modified: None, created: None })
    }

    fn read(&mut self, path: &str) -> Result<Vec<u8>, String> {
        vfs::read_file(path).map_err(err)
    }

    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), String> {
        vfs::write_file(path, data).map_err(err)
    }

    fn remove(&mut self, path: &str) -> Result<(), String> {
        vfs::remove(path).map_err(err)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
        vfs::rename(from, to).map_err(err)
    }

    fn copy(&mut self, from: &str, to: &str) -> Result<(), String> {
        vfs::copy(from, to).map_err(err)
    }

    fn mkdir(&mut self, path: &str) -> Result<(), String> {
        vfs::create_dir(path).map_err(err)
    }

    fn now(&mut self) -> i64 {
        crate::rtc::unix_seconds() as i64 * 1_000_000_000
    }

    fn confirm(&mut self, question: &str) -> bool {
        print!("{question} [y/N] ");
        loop {
            // Ctrl+C answers no (and is used up, so it doesn't also stop
            // whatever runs next).
            if keyboard::take_interrupt() {
                println!("n");
                return false;
            }
            match keyboard::read_char() {
                Some(b'y' | b'Y') => {
                    println!("y");
                    return true;
                }
                Some(b'n' | b'N' | b'\n') => {
                    println!("n");
                    return false;
                }
                Some(_) => {}
                None => task::sleep_ticks(1),
            }
        }
    }

    fn interrupted(&mut self) -> bool {
        keyboard::take_interrupt()
    }

    fn apex(&mut self, what: &str) -> bool {
        crate::apex::authenticate(what)
    }

    fn width(&mut self) -> usize {
        (console::CONSOLE.lock().cols() as usize).max(20)
    }
}

/// Commands ks has its own structured versions of.
const REPLACED: &[&str] = &["ls", "cd", "pwd", "cat", "rm", "echo", "clear", "help", "ps", "kill", "meminfo", "uptime"];

/// A ks engine with the kernel's commands added.
pub fn engine() -> Engine {
    let mut e = Engine::new();
    let s = "system";
    e.register(Signature::build("ps", s, "the running tasks: id, name, state, ticks, current"), Runner::Native(ps));
    e.register(
        Signature::build("kill", s, "stop tasks by id, or piped from ps: ps | filter name == run:elf | kill").rest("id", Shape::Int, "the tasks' ids"),
        Runner::Native(kill),
    );
    e.register(Signature::build("uptime", s, "how long since KonjacOS started"), Runner::Native(uptime));
    e.register(Signature::build("mem", s, "memory: physical and heap, used and free"), Runner::Native(mem));
    e.register(Signature::build("disks", s, "the mounted disks: mount, format, size, free"), Runner::Native(disks));
    e.register(Signature::build("clear", s, "clear the screen"), Runner::Native(clear));
    for c in commands::COMMANDS {
        if REPLACED.contains(&c.name) {
            continue;
        }
        let mut sig = Signature::build(c.name, s, c.summary).raw();
        if c.requires_apex {
            sig = sig.apex();
        }
        e.register(sig, Runner::Text(c.handler));
    }
    e
}

fn ps(_: &mut Ctx, _: &Call, _: Value) -> Result<Value, ShellError> {
    Ok(Value::List(
        task::list()
            .into_iter()
            .map(|(id, name, state, ticks, current)| {
                Value::Record(
                    Record::new()
                        .push("id", Value::Int(id as i64))
                        .push("name", Value::str(name))
                        .push("state", Value::str(state.label()))
                        .push("ticks", Value::Int(ticks as i64))
                        .push("current", Value::Bool(current)),
                )
            })
            .collect(),
    ))
}

fn kill(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let mut ids: Vec<i64> = call.positional.iter().filter_map(|(v, _)| v.as_int()).collect();
    let rows = match input {
        Value::List(l) => l,
        Value::Nothing => Vec::new(),
        v => alloc::vec![v],
    };
    for r in &rows {
        match r {
            Value::Int(i) => ids.push(*i),
            Value::Record(rec) => match rec.get("id") {
                Some(Value::Int(i)) => ids.push(*i),
                _ => return Err(ShellError::new("a row piped in needs an `id` column")),
            },
            other => return Err(ShellError::new("expected task ids").hint(other.a_type())),
        }
    }
    if ids.is_empty() {
        return Err(call.err("which tasks? give their ids, or pipe them in from `ps`"));
    }
    let shell = task::list().into_iter().find(|t| t.4).map(|t| t.0 as i64);
    for id in &ids {
        if Some(*id) == shell {
            return Err(call.err("that's the shell itself"));
        }
    }
    for id in ids {
        if !task::kill(id as u64) {
            let mut m = String::from("there's no running task ");
            let _ = core::fmt::Write::write_fmt(&mut m, format_args!("{id}"));
            return Err(call.err(m));
        }
    }
    Ok(Value::Nothing)
}

fn uptime(_: &mut Ctx, _: &Call, _: Value) -> Result<Value, ShellError> {
    Ok(Value::Duration((timer::ticks() as i64).saturating_mul(1_000_000_000 / timer::HZ as i64)))
}

fn mem(_: &mut Ctx, _: &Call, _: Value) -> Result<Value, ShellError> {
    let (total, free) = crate::pmm::stats();
    let frame = crate::pmm::FRAME_SIZE;
    let (heap_free, heap_mapped) = crate::heap::stats();
    let size = |b: u64| Value::Size(b as i64);
    Ok(Value::Record(
        Record::new()
            .push("total", size(total * frame))
            .push("used", size((total - free) * frame))
            .push("free", size(free * frame))
            .push("heap mapped", size(heap_mapped))
            .push("heap free", size(heap_free)),
    ))
}

fn disks(_: &mut Ctx, _: &Call, _: Value) -> Result<Value, ShellError> {
    let mut rows = Vec::new();
    let row = |mount: &str, format: &str, size: u64, free: u64| {
        Value::Record(Record::new().push("mount", Value::str(mount)).push("format", Value::str(format)).push("size", Value::Size(size as i64)).push("free", Value::Size(free as i64)))
    };
    if let Some((_, _, total, free)) = crate::kfs::info() {
        let b = crate::kfs::BLOCK as u64;
        rows.push(row("/", "KonjacFS", total * b, free * b));
    }
    if crate::fat16::mounted() {
        if let Ok(v) = crate::fat16::volume_stats() {
            let mount = if crate::kfs::mounted() { vfs::FAT_MOUNT } else { "/" };
            rows.push(row(mount, "FAT16", v.total_clusters * v.cluster_bytes, v.free_clusters * v.cluster_bytes));
        }
    }
    Ok(Value::List(rows))
}

fn clear(_: &mut Ctx, _: &Call, _: Value) -> Result<Value, ShellError> {
    console::CONSOLE.lock().clear();
    Ok(Value::Nothing)
}
