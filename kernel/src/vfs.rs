//! The one filesystem interface the rest of the kernel uses. It owns the
//! current directory, turns every path into an absolute, normalised one,
//! and sends it to the filesystem that holds it:
//!
//! - `/kfs/...` goes to KonjacFS (`kfs.rs`), when a KFS disk is attached.
//! - Everything else goes to FAT16 (`fat16.rs`), which is still `/`.
//!
//! The FAT16 driver only ever sees absolute paths from here, so its own
//! notion of a current directory stays at the root.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::SpinLock;
use crate::{fat16, kfs};

/// Where the KFS volume appears.
pub const KFS_MOUNT: &str = "/kfs";
const MOUNT_POINT: &str = "that's where the KFS disk is attached; it can't be moved or deleted";

pub use fat16::VolumeStats;

static CWD: SpinLock<String> = SpinLock::new(String::new());

pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Which filesystem a path is on, and the path within it.
enum On {
    Fat(String),
    Kfs(String),
}

/// `path` made absolute (against the current directory) with `.`, `..`
/// and repeated slashes resolved.
pub fn absolute(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let cwd = CWD.lock().clone();
    let base = if path.starts_with('/') { "" } else { cwd.as_str() };
    for part in base.split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let mut out = String::new();
    for p in &parts {
        out.push('/');
        out.push_str(p);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

fn on(path: &str) -> On {
    let abs = absolute(path);
    if kfs::mounted() {
        if abs == KFS_MOUNT {
            return On::Kfs(String::from("/"));
        }
        if let Some(rest) = abs.strip_prefix(KFS_MOUNT).filter(|r| r.starts_with('/')) {
            return On::Kfs(String::from(rest));
        }
    }
    On::Fat(abs)
}

/// Whether `path` itself can't be renamed, moved or deleted: it's where a
/// volume is attached. (What's inside it can be changed.)
pub fn read_only(path: &str) -> bool {
    kfs::mounted() && absolute(path) == KFS_MOUNT
}

pub fn cwd_path_string() -> String {
    let cwd = CWD.lock();
    if cwd.is_empty() { String::from("/") } else { cwd.clone() }
}

pub fn change_dir(path: &str) -> Result<(), &'static str> {
    let abs = absolute(path);
    match stat_path(&abs)? {
        (true, _) => {
            *CWD.lock() = if abs == "/" { String::new() } else { abs };
            Ok(())
        }
        _ => Err("not a directory"),
    }
}

pub fn list_dir(path: &str) -> Result<Vec<DirEntry>, &'static str> {
    match on(path) {
        On::Kfs(p) => Ok(kfs::list_dir(&p)?.into_iter().map(|e| DirEntry { name: e.name, is_dir: e.is_dir, size: e.size }).collect()),
        On::Fat(p) => {
            let mut v: Vec<DirEntry> = fat16::list_dir(&p)?
                .into_iter()
                .map(|e| DirEntry { name: e.name, is_dir: e.is_dir, size: e.size as u64 })
                .collect();
            // The KFS volume shows up as a folder in the root.
            if p == "/" && kfs::mounted() {
                v.push(DirEntry { name: String::from(&KFS_MOUNT[1..]), is_dir: true, size: 0 });
            }
            Ok(v)
        }
    }
}

pub fn list_current_dir() -> Result<Vec<DirEntry>, &'static str> {
    list_dir(".")
}

/// `(is_dir, size)` of whatever `path` names.
pub fn stat_path(path: &str) -> Result<(bool, u64), &'static str> {
    match on(path) {
        On::Kfs(p) => kfs::stat(&p),
        On::Fat(p) => fat16::stat_path(&p).map(|(d, s)| (d, s as u64)),
    }
}

pub fn read_file(path: &str) -> Result<Vec<u8>, &'static str> {
    match on(path) {
        On::Kfs(p) => kfs::read_file(&p),
        On::Fat(p) => fat16::read_file(&p),
    }
}

/// An open file, read at any offset.
#[derive(Clone, Copy)]
pub struct File {
    inner: Inner,
    /// Size in bytes (files on these volumes are under 4 GiB).
    pub size: u32,
}

#[derive(Clone, Copy)]
enum Inner {
    Fat(fat16::File),
    Kfs(kfs::File),
}

pub fn open_file(path: &str) -> Result<File, &'static str> {
    match on(path) {
        On::Kfs(p) => {
            let f = kfs::open_file(&p)?;
            Ok(File { size: f.size.min(u32::MAX as u64) as u32, inner: Inner::Kfs(f) })
        }
        On::Fat(p) => {
            let f = fat16::open_file(&p)?;
            Ok(File { size: f.size, inner: Inner::Fat(f) })
        }
    }
}

impl File {
    pub fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, &'static str> {
        match &mut self.inner {
            Inner::Fat(f) => f.read_at(offset, out),
            Inner::Kfs(f) => f.read_at(offset, out),
        }
    }
}

pub fn read_file_range(path: &str, offset: u32, len: usize) -> Result<Vec<u8>, &'static str> {
    let mut file = open_file(path)?;
    let size = len.min(file.size.saturating_sub(offset) as usize);
    let mut out = Vec::new();
    out.try_reserve_exact(size).map_err(|_| "file buffer allocation failed")?;
    out.resize(size, 0);
    file.read_at(offset as u64, &mut out)?;
    Ok(out)
}

pub fn write_file(path: &str, data: &[u8]) -> Result<(), &'static str> {
    match on(path) {
        On::Kfs(p) => kfs::write_file(&p, data),
        On::Fat(p) => fat16::write_file(&p, data),
    }
}

pub fn create_dir(path: &str) -> Result<(), &'static str> {
    match on(path) {
        On::Kfs(p) => kfs::create_dir(&p),
        On::Fat(p) => fat16::create_dir(&p),
    }
}

pub fn remove_file(path: &str) -> Result<(), &'static str> {
    if read_only(path) {
        return Err(MOUNT_POINT);
    }
    match on(path) {
        On::Kfs(p) => kfs::remove_file(&p),
        On::Fat(p) => fat16::remove_file(&p),
    }
}

/// Deletes a file, or a folder with everything in it.
pub fn remove(path: &str) -> Result<(), &'static str> {
    if read_only(path) {
        return Err(MOUNT_POINT);
    }
    match on(path) {
        On::Kfs(p) => kfs::remove(&p),
        On::Fat(p) => fat16::remove(&p),
    }
}

/// Renames or moves a file or folder. Between volumes that means copying
/// it, then deleting the original.
pub fn rename(from: &str, to: &str) -> Result<(), &'static str> {
    if read_only(from) || read_only(to) {
        return Err(MOUNT_POINT);
    }
    match (on(from), on(to)) {
        (On::Fat(a), On::Fat(b)) => fat16::rename(&a, &b),
        (On::Kfs(a), On::Kfs(b)) => kfs::rename(&a, &b),
        _ => {
            if stat_path(to).is_ok() {
                return Err("something with that name is already there");
            }
            if let Err(e) = copy_across(&absolute(from), &absolute(to), 0) {
                // Don't leave half a copy behind.
                let _ = remove(to);
                return Err(e);
            }
            remove(from)
        }
    }
}

/// Copies a file, or a folder with everything in it, to a new path --
/// across volumes too.
pub fn copy(from: &str, to: &str) -> Result<(), &'static str> {
    match (on(from), on(to)) {
        (On::Fat(a), On::Fat(b)) => fat16::copy(&a, &b),
        _ => {
            let (from, to) = (absolute(from), absolute(to));
            if to.strip_prefix(from.as_str()).is_some_and(|rest| rest.starts_with('/')) {
                return Err("can't copy a folder into itself");
            }
            copy_across(&from, &to, 0)
        }
    }
}

fn copy_across(from: &str, to: &str, depth: u32) -> Result<(), &'static str> {
    if depth > 32 {
        return Err("folders nested too deeply");
    }
    if stat_path(to).is_ok() {
        return Err("something with that name is already there");
    }
    let (is_dir, _) = stat_path(from)?;
    if !is_dir {
        let data = read_file(from)?;
        return write_file(to, &data);
    }
    create_dir(to)?;
    for e in list_dir(from)? {
        let join = |base: &str| {
            let mut p = String::from(base);
            if !p.ends_with('/') {
                p.push('/');
            }
            p.push_str(&e.name);
            p
        };
        copy_across(&join(from), &join(to), depth + 1)?;
    }
    Ok(())
}

/// Space on the root volume, for `statfs` and Settings.
pub fn volume_stats() -> Result<VolumeStats, &'static str> {
    fat16::volume_stats()
}
