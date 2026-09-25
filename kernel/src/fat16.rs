//! A read-only FAT16 driver, layered on `ata.rs`'s raw sector reads.
//!
//! FAT16 was picked over a custom filesystem specifically so the disk
//! image stays a completely ordinary FAT16 volume: it's built with real
//! host tools (`mkfs.vfat`/`mtools`), and any FAT-aware tool -- including
//! just mounting it on the host -- can add or inspect files on it. The
//! driver supports 8.3 short filenames, reading (not yet writing) real
//! VFAT long filenames, reading and writing whole files, and walking
//! subdirectories (including a `cd`-style current-directory tracked here
//! rather than in the shell, so any future caller besides the shell gets
//! the same notion of "where we are"). Creating/removing directories and
//! timestamps are still unsupported -- writes always store a zeroed
//! date/time, which real FAT tools treat as "unknown," not an error, and
//! every write always targets an already-existing short-name entry, never
//! allocating new LFN entries of its own.
//!
//! Subdirectory traversal deliberately does *not* rely on the on-disk "."
//! and ".." entries every FAT directory has -- [`Cwd`] keeps its own
//! stack of (name, cluster) instead. That's what lets [`cwd_path_string`]
//! print a real path (FAT doesn't store a directory's name or parent
//! anywhere *in* the directory itself, only in whichever parent entry
//! points at it) and lets `cd ..` be a plain stack pop instead of an
//! extra disk read.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::ata;
use crate::sync::SpinLock;

const SECTOR_SIZE: usize = ata::SECTOR_SIZE;
const DIR_ENTRY_SIZE: usize = 32;

const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_LFN: u8 = 0x0F; // attr byte reads as this (ORed with itself) for LFN entries.

const ENTRY_FREE: u8 = 0x00;
const ENTRY_DELETED: u8 = 0xE5;
const ENTRY_DOT: u8 = b'.'; // First byte of "." and ".." entries.

#[derive(Clone, Copy)]
#[allow(dead_code)] // Kept for completeness/debugging even though not every field is read yet.
struct Layout {
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    fat_start_lba: u32,
    fat_sectors: u32,
    num_fats: u32,
    root_dir_start_lba: u32,
    root_dir_sectors: u32,
    root_entry_count: u32,
    data_start_lba: u32,
    /// Total sectors on the volume (whichever of the BPB's 16-bit/32-bit
    /// total-sectors fields is actually populated) -- needed to know how
    /// many clusters exist at all, i.e. where [`allocate_cluster`] has to
    /// stop looking.
    total_sectors: u32,
}

/// Where a directory's entries live: the root directory has its own fixed
/// region (a FAT16 quirk -- unlike every other directory, it isn't a
/// cluster chain), everything else is a normal cluster chain starting at
/// the given cluster.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DirLocation {
    Root,
    Cluster(u32),
}

/// The shell's (or anyone's) current directory: both a resolved
/// [`DirLocation`] to actually read, and the stack of names that got us
/// there, for [`cwd_path_string`] and for resolving `..` without touching
/// the disk.
struct Cwd {
    location: DirLocation,
    /// (name, cluster) for each path component from the root down. Empty
    /// means "at the root".
    stack: Vec<(String, u32)>,
}

// File faults run with IRQs masked: task-context readers must not be preempted
// while holding this lock by another task that faults and needs the layout.
static LAYOUT: crate::sync::IrqSpinLock<Option<Layout>> = crate::sync::IrqSpinLock::new(None);
static CWD: SpinLock<Cwd> = SpinLock::new(Cwd { location: DirLocation::Root, stack: Vec::new() });
/// Cheap instrumentation for `meminfo`/debugging: total sectors read since
/// boot, so it's obvious whether `ls`/`cat` are actually hitting the disk.
static SECTORS_READ: AtomicU64 = AtomicU64::new(0);
static SECTORS_WRITTEN: AtomicU64 = AtomicU64::new(0);

fn read_sector(lba: u32) -> Result<[u8; SECTOR_SIZE], &'static str> {
    let mut buf = [0u8; SECTOR_SIZE];
    unsafe { ata::read_sector(lba, &mut buf) }?;
    SECTORS_READ.fetch_add(1, Ordering::Relaxed);
    Ok(buf)
}

fn write_sector(lba: u32, buf: &[u8; SECTOR_SIZE]) -> Result<(), &'static str> {
    unsafe { ata::write_sector(lba, buf) }?;
    SECTORS_WRITTEN.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Parses the boot sector / BIOS Parameter Block and caches the derived
/// layout for every later call in this module. Also resets the current
/// directory to root, in case this is ever called more than once.
///
/// # Safety
/// Must be called after the ATA driver is usable (no init needed there
/// beyond the kernel being far enough into boot to do port I/O) and before
/// any other function in this module.
pub unsafe fn init() -> Result<(), &'static str> {
    let boot = read_sector(0)?;

    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err("fat16: missing boot sector signature (no filesystem on disk?)");
    }

    let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]) as u32;
    let sectors_per_cluster = boot[13] as u32;
    let reserved_sectors = u16::from_le_bytes([boot[14], boot[15]]) as u32;
    let num_fats = boot[16] as u32;
    let root_entry_count = u16::from_le_bytes([boot[17], boot[18]]) as u32;
    let total_sectors_16 = u16::from_le_bytes([boot[19], boot[20]]) as u32;
    let fat_sectors_16 = u16::from_le_bytes([boot[22], boot[23]]) as u32;
    let total_sectors_32 = u32::from_le_bytes([boot[32], boot[33], boot[34], boot[35]]);

    if bytes_per_sector as usize != SECTOR_SIZE {
        return Err("fat16: only 512-byte sectors are supported");
    }
    if fat_sectors_16 == 0 {
        return Err("fat16: FAT32 volume (or malformed BPB) -- not supported");
    }
    let total_sectors = if total_sectors_16 != 0 { total_sectors_16 } else { total_sectors_32 };

    let fat_start_lba = reserved_sectors;
    let root_dir_start_lba = fat_start_lba + num_fats * fat_sectors_16;
    let root_dir_bytes = root_entry_count * DIR_ENTRY_SIZE as u32;
    let root_dir_sectors = root_dir_bytes.div_ceil(bytes_per_sector);
    let data_start_lba = root_dir_start_lba + root_dir_sectors;

    let layout = Layout {
        bytes_per_sector,
        sectors_per_cluster,
        fat_start_lba,
        fat_sectors: fat_sectors_16,
        num_fats,
        root_dir_start_lba,
        root_dir_sectors,
        root_entry_count,
        data_start_lba,
        total_sectors,
    };

    *LAYOUT.lock() = Some(layout);
    *CWD.lock() = Cwd { location: DirLocation::Root, stack: Vec::new() };
    Ok(())
}

fn layout() -> Result<Layout, &'static str> {
    LAYOUT.lock().ok_or("fat16: not initialized (init() failed or wasn't called)")
}

fn cluster_to_lba(l: Layout, cluster: u32) -> u32 {
    l.data_start_lba + (cluster - 2) * l.sectors_per_cluster
}

/// Looks up FAT entry `cluster`'s value (the next cluster in the chain, or
/// an end-of-chain/free/bad marker).
fn fat_entry(l: Layout, cluster: u32) -> Result<u16, &'static str> {
    let byte_offset = cluster * 2;
    let sector = l.fat_start_lba + byte_offset / l.bytes_per_sector;
    let offset_in_sector = (byte_offset % l.bytes_per_sector) as usize;
    let buf = read_sector(sector)?;
    Ok(u16::from_le_bytes([buf[offset_in_sector], buf[offset_in_sector + 1]]))
}

/// Yields every sector (as an LBA) that makes up a directory's entries,
/// whether that's the root directory's fixed region or an ordinary
/// cluster chain. A disk read error or a FAT read error partway through a
/// chain just ends the iteration early (callers see however much was
/// found before the error, not the error itself) -- acceptable for a
/// read-only hobby driver, since a mid-chain error means the disk is
/// already in worse trouble than one truncated listing.
struct DirSectors {
    layout: Layout,
    state: DirSectorsState,
}

enum DirSectorsState {
    Root { next: u32, end: u32 },
    Chain { cluster: u32, sector_in_cluster: u32 },
    Done,
}

impl DirSectors {
    fn new(layout: Layout, location: DirLocation) -> Self {
        let state = match location {
            DirLocation::Root => DirSectorsState::Root { next: layout.root_dir_start_lba, end: layout.root_dir_start_lba + layout.root_dir_sectors },
            DirLocation::Cluster(c) => DirSectorsState::Chain { cluster: c, sector_in_cluster: 0 },
        };
        DirSectors { layout, state }
    }
}

impl Iterator for DirSectors {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        match &mut self.state {
            DirSectorsState::Root { next, end } => {
                if *next >= *end {
                    return None;
                }
                let lba = *next;
                *next += 1;
                Some(lba)
            }
            DirSectorsState::Chain { cluster, sector_in_cluster } => {
                if *cluster < 2 || *cluster >= 0xFFF8 {
                    self.state = DirSectorsState::Done;
                    return None;
                }
                let lba = cluster_to_lba(self.layout, *cluster) + *sector_in_cluster;
                *sector_in_cluster += 1;
                if *sector_in_cluster >= self.layout.sectors_per_cluster {
                    *sector_in_cluster = 0;
                    *cluster = fat_entry(self.layout, *cluster).map(u32::from).unwrap_or(0xFFFF);
                }
                Some(lba)
            }
            DirSectorsState::Done => None,
        }
    }
}

/// One resolved entry from a directory: a human-readable "NAME.EXT" (or
/// just "NAME" with no extension), whether it's a subdirectory, its size
/// in bytes (0 for directories), and its starting cluster (needed to
/// actually descend into it if it's a directory, or read it if it's a
/// file spread across more than one cluster).
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u32,
    pub cluster: u32,
}

/// Turns the raw 8.3 `name[8]`/`ext[3]` fields into a normal display
/// string, trimming the space-padding FAT uses and skipping the dot
/// entirely when there's no extension.
fn format_short_name(raw: &[u8]) -> String {
    let name = &raw[0..8];
    let ext = &raw[8..11];
    let name_end = name.iter().rposition(|&b| b != b' ').map_or(0, |i| i + 1);
    let ext_end = ext.iter().rposition(|&b| b != b' ').map_or(0, |i| i + 1);

    let mut out = String::new();
    for &b in &name[..name_end] {
        out.push(b as char);
    }
    if ext_end > 0 {
        out.push('.');
        for &b in &ext[..ext_end] {
            out.push(b as char);
        }
    }
    out
}

/// Decodes one raw 32-byte directory entry into a [`DirEntry`]. Callers
/// are responsible for having already filtered out free/deleted/LFN/dot
/// entries -- this just extracts fields, it doesn't judge them.
fn decode_entry(chunk: &[u8]) -> DirEntry {
    let attr = chunk[11];
    let cluster = u16::from_le_bytes([chunk[26], chunk[27]]) as u32;
    let size = u32::from_le_bytes([chunk[28], chunk[29], chunk[30], chunk[31]]);
    DirEntry {
        name: format_short_name(&chunk[0..11]),
        is_dir: attr & ATTR_DIRECTORY != 0,
        size,
        cluster,
    }
}

/// Whether a raw entry's attribute byte marks it as something
/// [`list_dir_at`]/[`locate_slot`] should skip over entirely (the volume
/// label) rather than treat as a real file or directory. LFN entries
/// aren't "skipped" any more -- see [`list_dir_at`] -- so they're no
/// longer part of this check; this only still matters for the on-disk
/// volume-label entry, which is a real attribute-marked entry but never a
/// file or directory either.
fn is_skippable_attr(attr: u8) -> bool {
    attr & ATTR_VOLUME_ID != 0
}

/// One VFAT long-filename entry's 13 UTF-16LE characters, decoded in
/// on-disk field order (name1[5] + name2[6] + name3[2] -- see the on-disk
/// layout this reads from). Doesn't stop at a NUL/0xFFFF padding char
/// itself; [`reconstruct_long_name`] does that once every entry's chars
/// are concatenated in the right order, since a NUL can only be trusted to
/// mean "end of name" once it's known which physical entry holds the
/// *last* part of the name (the one with the `0x40` "last entry" bit set).
fn decode_lfn_chars(chunk: &[u8]) -> [u16; 13] {
    let mut chars = [0u16; 13];
    let word = |off: usize| u16::from_le_bytes([chunk[off], chunk[off + 1]]);
    for i in 0..5 {
        chars[i] = word(1 + i * 2);
    }
    for i in 0..6 {
        chars[5 + i] = word(14 + i * 2);
    }
    for i in 0..2 {
        chars[11 + i] = word(28 + i * 2);
    }
    chars
}

/// Reassembles a full VFAT long filename from its accumulated entries.
/// `parts` holds every LFN entry seen immediately before the real 8.3
/// entry that names this file, as `(sequence_number, chars)` -- real
/// on-disk order is *highest* sequence number (the entry holding the
/// *last* part of the name) first, working down to sequence 1 (the first
/// 13 characters) right before the short entry, so this sorts by sequence
/// number ascending before concatenating, then stops at the first NUL
/// (`0x0000`) code unit -- a short name's LFN doesn't necessarily fill a
/// whole 13-character entry, and the remainder is padded with `0xFFFF`,
/// not more NULs, so trimming at the first NUL is both correct and
/// sufficient. Only handles the Basic Multilingual Plane one code unit at
/// a time (no surrogate pairs) -- real filenames on this project's own
/// disk images are plain ASCII, and a lossy `?` for anything outside that
/// is an honest degradation, not silent data loss on the names this
/// driver actually needs to read.
fn reconstruct_long_name(parts: &mut Vec<(u8, [u16; 13])>) -> String {
    parts.sort_by_key(|&(seq, _)| seq);
    let mut out = String::new();
    'parts: for &(_, chars) in parts.iter() {
        for &c in &chars {
            if c == 0x0000 {
                break 'parts;
            }
            out.push(char::from_u32(c as u32).unwrap_or('?'));
        }
    }
    out
}

/// Lists every real file/directory in `location`, skipping deleted
/// entries, the volume-label entry, and the "." / ".." entries every
/// non-root directory has (this driver tracks the current directory
/// itself -- see [`Cwd`] -- so those two are just noise here, not
/// something callers need to see or handle). VFAT long-filename entries
/// (`ATTR_LFN`) are read, not skipped: each one is accumulated into
/// `lfn_parts`, and the moment the real 8.3 entry they belong to shows up,
/// [`reconstruct_long_name`] turns the accumulated parts into this
/// [`DirEntry`]'s real display name instead of the short name
/// `decode_entry` would otherwise have used -- real long names, read
/// straight off a real VFAT-formatted disk image (built by ordinary host
/// `mtools`/`mcopy`, which always writes proper LFN entries for a name
/// that doesn't fit 8.3), not something this driver has to fabricate or
/// approximate. A deleted entry clears any parts accumulated so far,
/// rather than letting them leak onto whatever real entry happens to
/// follow a deleted run -- a real on-disk possibility this driver hasn't
/// had to consider until names could span more than one entry at all.
fn list_dir_at(location: DirLocation) -> Result<Vec<DirEntry>, &'static str> {
    let l = layout()?;
    let mut out = Vec::new();
    let mut lfn_parts: Vec<(u8, [u16; 13])> = Vec::new();

    'sectors: for lba in DirSectors::new(l, location) {
        let buf = read_sector(lba)?;
        for chunk in buf.chunks_exact(DIR_ENTRY_SIZE) {
            let first = chunk[0];
            if first == ENTRY_FREE {
                break 'sectors; // No more entries at all, ever.
            }
            if first == ENTRY_DELETED {
                lfn_parts.clear();
                continue;
            }
            if chunk[11] == ATTR_LFN {
                let seq = first & 0x1F; // Mask off the 0x40 "last entry"/0x80 reserved bits -- only the ordering number matters for reassembly.
                lfn_parts.push((seq, decode_lfn_chars(chunk)));
                continue;
            }
            if first == ENTRY_DOT {
                lfn_parts.clear();
                continue;
            }
            if is_skippable_attr(chunk[11]) {
                lfn_parts.clear();
                continue;
            }
            let mut entry = decode_entry(chunk);
            if !lfn_parts.is_empty() {
                entry.name = reconstruct_long_name(&mut lfn_parts);
            }
            lfn_parts.clear();
            out.push(entry);
        }
    }

    Ok(out)
}

/// Lists the current directory (see [`cwd_path_string`]).
pub fn list_current_dir() -> Result<Vec<DirEntry>, &'static str> {
    list_dir_at(CWD.lock().location)
}

/// Formats an arbitrary display name (`readme.txt`, `README`, ...) into
/// FAT's fixed 8.3 on-disk form for comparison -- uppercased, space
/// padded, truncated to 8+3.
fn to_short_name(display: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    let (base, ext) = match display.rsplit_once('.') {
        Some((b, e)) => (b, e),
        None => (display, ""),
    };
    for (i, b) in base.bytes().take(8).enumerate() {
        out[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.bytes().take(3).enumerate() {
        out[8 + i] = b.to_ascii_uppercase();
    }
    out
}

/// Case-insensitive full-name match against whatever [`list_dir_at`]
/// resolved each entry's display name to -- a real long name when one was
/// there to reconstruct, the plain short name otherwise. Used to compare
/// via `to_short_name`-on-both-sides (which happened to still find the
/// right entry for a short name, since truncating-then-comparing two
/// equal short names is a no-op, but would have silently mismatched, or
/// worse, mismatched a *different* file, for any real long name) --
/// replaced once long names became something this driver could actually
/// produce, since two long names that only differ after the 8.3 truncation
/// point would otherwise have looked identical to it.
fn find_in_dir(location: DirLocation, name: &str) -> Result<DirEntry, &'static str> {
    list_dir_at(location)?
        .into_iter()
        .find(|e| e.name.eq_ignore_ascii_case(name))
        .ok_or("no such file or directory")
}

/// Walks a `/`-separated path (relative to `(location, stack)`, or from
/// root if `path` starts with `/`) and returns where it lands, as both a
/// [`DirLocation`] to actually read and the updated name stack (for
/// [`cwd_path_string`]-style display). `..` is handled by popping the
/// stack rather than reading an on-disk ".." entry -- see the module docs
/// for why. Shared by [`change_dir`] (which commits the result as the new
/// cwd) and [`resolve_dir`] (which just wants the endpoint).
fn walk_path(location: DirLocation, stack: Vec<(String, u32)>, path: &str) -> Result<(DirLocation, Vec<(String, u32)>), &'static str> {
    let mut location = if path.starts_with('/') { DirLocation::Root } else { location };
    let mut stack = if path.starts_with('/') { Vec::new() } else { stack };

    for part in path.split('/').filter(|p| !p.is_empty()) {
        match part {
            "." => continue,
            ".." => {
                stack.pop();
                location = match stack.last() {
                    Some((_, cluster)) => DirLocation::Cluster(*cluster),
                    None => DirLocation::Root,
                };
            }
            name => {
                let entry = find_in_dir(location, name)?;
                if !entry.is_dir {
                    return Err("not a directory");
                }
                stack.push((entry.name.clone(), entry.cluster));
                location = DirLocation::Cluster(entry.cluster);
            }
        }
    }

    Ok((location, stack))
}

/// Resolves a path to the [`DirLocation`] it names, relative to the
/// current directory, without changing it. Used by [`read_file`] to
/// resolve a path's directory portion.
fn resolve_dir(path: &str) -> Result<DirLocation, &'static str> {
    let cwd = CWD.lock();
    let (location, stack) = (cwd.location, cwd.stack.clone());
    drop(cwd);
    walk_path(location, stack, path).map(|(loc, _)| loc)
}

/// Changes the current directory, same path syntax `resolve_dir` accepts:
/// `/`-separated, `.`/`..`, absolute (leading `/`) or relative.
pub fn change_dir(path: &str) -> Result<(), &'static str> {
    let cwd_before = { let cwd = CWD.lock(); (cwd.location, cwd.stack.clone()) };
    let (location, stack) = walk_path(cwd_before.0, cwd_before.1, path)?;

    let mut cwd = CWD.lock();
    cwd.location = location;
    cwd.stack = stack;
    Ok(())
}

/// The current directory as a human-readable path, e.g. `/DOCS/SUB`, or
/// `/` at the root.
pub fn cwd_path_string() -> String {
    let cwd = CWD.lock();
    if cwd.stack.is_empty() {
        return String::from("/");
    }
    let mut out = String::new();
    for (name, _) in &cwd.stack {
        out.push('/');
        out.push_str(name);
    }
    out
}

/// Splits a path into `(directory portion, filename portion)` for feeding
/// the directory portion to [`resolve_dir`]/[`walk_path`]. A single
/// no-op case matters here: for a single-component absolute path like
/// `/FOO.TXT`, a plain `rsplit_once('/')` gives an *empty* directory
/// portion -- but `resolve_dir("")` means "relative to cwd", not "root",
/// so without this fixup an absolute single-component path would
/// silently resolve against whatever directory the caller happened to be
/// in instead of root. This preserves the leading `/` in that case so it
/// resolves correctly regardless of the caller's current directory.
fn split_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((d, f)) if d.is_empty() && path.starts_with('/') => ("/", f),
        Some((d, f)) => (d, f),
        None => ("", path),
    }
}

/// Looks up whatever `path` names -- file *or* directory -- without ever
/// reading a file's contents, unlike [`read_file`] (which actively refuses
/// a directory outright). Returns `(is_dir, size)`; `size` is `0` for a
/// directory (this driver has no notion of a directory's own on-disk size
/// worth reporting, same as real Linux's `stat` reporting a directory's
/// size as its own block usage, not something callers here care about).
///
/// This exists for a real reason, not symmetry: real Linux `fstatat`/`stat`
/// can stat a directory (`S_ISDIR`), and a real glibc `ld.so` genuinely
/// depends on that -- its own `RPATH`/`RUNPATH` directory-existence caching
/// (`_dl_map_object`'s `open_path`) `fstatat`s each search directory once
/// to decide whether to keep trying files in it, and *caches a negative
/// result for the rest of the process* if that stat fails. Before this
/// function existed, `linux_syscall.rs`'s `sys_newfstatat` had nothing to
/// call but [`read_file`], which honestly errors on a directory target --
/// but "honestly can't read a directory as a file" and "this directory
/// doesn't exist" are different facts, and conflating them into the same
/// `ENOENT` answer fed real, existing directories into that cache as
/// permanently nonexisting, silently breaking every *later* dependency's
/// library search on this exact disk layout even though the directory was
/// right there the whole time. Two real, existing candidates first tried
/// via `resolve_dir` (a directory path, trailing slash or not) and, only
/// if that fails, `resolve_dir`+`find_in_dir` on the split (directory,
/// filename) pair (an ordinary file path) -- covers both shapes a real
/// caller asks about with one function.
pub fn stat_path(path: &str) -> Result<(bool, u32), &'static str> {
    if let Ok(_dir) = resolve_dir(path) {
        return Ok((true, 0));
    }
    let (dir_part, filename) = split_path(path);
    if filename.is_empty() {
        return Err("no such file or directory");
    }
    let dir_location = resolve_dir(dir_part)?;
    let entry = find_in_dir(dir_location, filename)?;
    Ok((entry.is_dir, entry.size))
}

/// Reads an entire file by path (case-insensitive 8.3 names; `/`-separated,
/// absolute or relative to the current directory -- see [`resolve_dir`]).
pub fn read_file(path: &str) -> Result<Vec<u8>, &'static str> {
    let l = layout()?;
    let (dir_part, filename) = split_path(path);
    if filename.is_empty() {
        return Err("not a file");
    }
    let dir_location = resolve_dir(dir_part)?;
    let entry = find_in_dir(dir_location, filename)?;
    if entry.is_dir {
        return Err("is a directory");
    }

    let mut cluster = entry.cluster;
    let size = entry.size;
    let mut data = Vec::with_capacity(size as usize);

    while cluster >= 2 && cluster < 0xFFF8 && (data.len() as u32) < size {
        for s in 0..l.sectors_per_cluster {
            let sector_lba = cluster_to_lba(l, cluster) + s;
            let buf = read_sector(sector_lba)?;
            let remaining = size - data.len() as u32;
            let take = core::cmp::min(remaining, l.bytes_per_sector) as usize;
            data.extend_from_slice(&buf[..take]);
            if (data.len() as u32) >= size {
                break;
            }
        }
        cluster = fat_entry(l, cluster)? as u32;
    }

    Ok(data)
}

/// A resolved read-only handle. Copying it retains the original file identity
/// across cwd changes and descriptor close. The filesystem must not overwrite
/// or delete the file while handles/mappings exist (no unlink pinning yet).
#[derive(Clone, Copy)]
pub struct File {
    first: u32,
    pub size: u32,
    cursor_cluster: u32,
    cursor_index: u32,
}

pub fn open_file(path: &str) -> Result<File, &'static str> {
    let (dir, name) = split_path(path);
    if name.is_empty() { return Err("not a file"); }
    let entry = find_in_dir(resolve_dir(dir)?, name)?;
    if entry.is_dir { return Err("is a directory"); }
    Ok(File { first: entry.cluster, size: entry.size,
        cursor_cluster: entry.cluster, cursor_index: 0 })
}

impl File {
    /// Fill only resident kernel memory. No allocation or user access occurs.
    /// The cached chain position accelerates forward reads; backwards reads
    /// restart at the first cluster. All offset arithmetic stays at u64 width.
    pub fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, &'static str> {
        if offset >= self.size as u64 || out.is_empty() { return Ok(0); }
        let l = layout()?;
        let cluster_bytes = l.sectors_per_cluster as u64 * SECTOR_SIZE as u64;
        if cluster_bytes == 0 { return Err("invalid cluster size"); }
        let data_sectors = l.total_sectors.checked_sub(l.data_start_lba).ok_or("invalid data region")?;
        let max_clusters = data_sectors / l.sectors_per_cluster;
        let fat_entries = l.fat_sectors as u64 * SECTOR_SIZE as u64 / 2;
        let valid = |c: u32| c >= 2 && c < 0xfff0 && c - 2 < max_clusters && (c as u64) < fat_entries;
        let target = (offset / cluster_bytes) as u32;
        if target >= max_clusters { return Err("invalid file chain length"); }
        let (mut cluster, mut index) = if target >= self.cursor_index {
            (self.cursor_cluster, self.cursor_index)
        } else { (self.first, 0) };
        // Cache one FAT sector per read: large random offsets must not issue
        // one ATA command for every two-byte FAT entry they traverse.
        let mut fat_sector = [0u8; SECTOR_SIZE];
        let mut fat_lba = u32::MAX;
        let mut next = |c: u32| -> Result<u32, &'static str> {
            if !valid(c) { return Err("truncated or invalid file chain"); }
            let sector = l.fat_start_lba + c * 2 / SECTOR_SIZE as u32;
            if sector != fat_lba { fat_sector = read_sector(sector)?; fat_lba = sector; }
            let at = (c * 2 % SECTOR_SIZE as u32) as usize;
            Ok(u16::from_le_bytes([fat_sector[at], fat_sector[at+1]]) as u32)
        };
        while index < target { cluster = next(cluster)?; index += 1; }
        let want = out.len().min((self.size as u64 - offset) as usize);
        let mut done = 0;
        let mut within = offset % cluster_bytes;
        while done < want {
            if !valid(cluster) || index >= max_clusters { return Err("truncated or invalid file chain"); }
            self.cursor_cluster = cluster;
            self.cursor_index = index;
            let sector = read_sector(cluster_to_lba(l, cluster) + (within / SECTOR_SIZE as u64) as u32)?;
            let lo = (within % SECTOR_SIZE as u64) as usize;
            let take = (SECTOR_SIZE - lo).min(want - done);
            out[done..done+take].copy_from_slice(&sector[lo..lo+take]);
            done += take;
            within += take as u64;
            if within == cluster_bytes && done < want {
                cluster = next(cluster)?; index += 1; within = 0;
            }
        }
        Ok(done)
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

// --- Writing ---------------------------------------------------------
//
// Everything below here mutates the disk. The overall shape is the same
// for both creating/overwriting a file and deleting one: find or claim a
// slot in the target directory (`locate_slot`), and manage the file's
// cluster chain in the FAT (`allocate_cluster`/`free_chain`, both of
// which keep every FAT copy in sync via `set_fat_entry` -- real FAT
// volumes have `num_fats` mirrored copies specifically so a tool like
// `fsck.vfat` can cross-check them, so it's worth keeping that property
// even though this driver itself only ever reads FAT copy 0).

const FAT_FREE: u16 = 0x0000;
const FAT_EOC: u16 = 0xFFFF;

/// Writes cluster `cluster`'s FAT entry to `value`, in every FAT copy the
/// volume has.
fn set_fat_entry(l: Layout, cluster: u32, value: u16) -> Result<(), &'static str> {
    let byte_offset = cluster * 2;
    let offset_in_sector = (byte_offset % l.bytes_per_sector) as usize;
    let sector_in_fat = byte_offset / l.bytes_per_sector;

    for fat_index in 0..l.num_fats {
        let sector = l.fat_start_lba + fat_index * l.fat_sectors + sector_in_fat;
        let mut buf = read_sector(sector)?;
        buf[offset_in_sector] = (value & 0xFF) as u8;
        buf[offset_in_sector + 1] = (value >> 8) as u8;
        write_sector(sector, &buf)?;
    }
    Ok(())
}

/// Finds a free cluster (FAT entry `0x0000`), immediately marks it
/// end-of-chain so nothing else can claim it before the caller links it
/// in, and returns its number. A linear scan -- fine for a small hobby
/// disk, not something you'd want on a real multi-gigabyte volume.
fn allocate_cluster(l: Layout) -> Result<u32, &'static str> {
    let total_clusters = (l.total_sectors - l.data_start_lba) / l.sectors_per_cluster;
    for cluster in 2..2 + total_clusters {
        if fat_entry(l, cluster)? == FAT_FREE {
            set_fat_entry(l, cluster, FAT_EOC)?;
            return Ok(cluster);
        }
    }
    Err("disk full (no free clusters)")
}

/// Frees every cluster in the chain starting at `cluster`.
fn free_chain(l: Layout, mut cluster: u32) -> Result<(), &'static str> {
    while cluster >= 2 && cluster < 0xFFF8 {
        let next = fat_entry(l, cluster)?;
        set_fat_entry(l, cluster, FAT_FREE)?;
        cluster = next as u32;
    }
    Ok(())
}

/// Zero-fills every sector of one cluster -- used when extending a
/// directory with a freshly allocated cluster, so it reads back as "all
/// entries free" rather than whatever garbage was on disk before.
fn zero_cluster(l: Layout, cluster: u32) -> Result<(), &'static str> {
    let zero = [0u8; SECTOR_SIZE];
    for s in 0..l.sectors_per_cluster {
        write_sector(cluster_to_lba(l, cluster) + s, &zero)?;
    }
    Ok(())
}

/// Finds where a directory entry named `target` (an already-8.3-formatted
/// name) either already lives, or should be written: an exact name match
/// (for overwriting), a deleted/free slot (for a new entry), or -- for a
/// subdirectory that's completely full of live entries and has never hit
/// its own free-slot marker -- a freshly allocated and linked cluster to
/// extend it with. The root directory can't be extended this way (it's a
/// fixed-size region, a FAT16 quirk), so a full root directory is an
/// error instead.
///
/// Returns `(sector_lba, offset_within_sector, existing_entry)` --
/// `existing_entry` is `Some` only for the exact-name-match case.
fn locate_slot(l: Layout, location: DirLocation, target: &[u8; 11]) -> Result<(u32, usize, Option<DirEntry>), &'static str> {
    let mut free_slot: Option<(u32, usize)> = None;
    let mut cluster = match location {
        DirLocation::Root => None,
        DirLocation::Cluster(c) => Some(c),
    };

    loop {
        let sector_range: Vec<u32> = match (location, cluster) {
            (DirLocation::Root, _) => (0..l.root_dir_sectors).map(|i| l.root_dir_start_lba + i).collect(),
            (DirLocation::Cluster(_), Some(c)) => (0..l.sectors_per_cluster).map(|s| cluster_to_lba(l, c) + s).collect(),
            (DirLocation::Cluster(_), None) => Vec::new(),
        };

        for lba in sector_range {
            let buf = read_sector(lba)?;
            for (idx, chunk) in buf.chunks_exact(DIR_ENTRY_SIZE).enumerate() {
                let first = chunk[0];
                let offset = idx * DIR_ENTRY_SIZE;
                if first == ENTRY_FREE {
                    return Ok((free_slot.map(|s| s.0).unwrap_or(lba), free_slot.map(|s| s.1).unwrap_or(offset), None));
                }
                if first == ENTRY_DELETED {
                    if free_slot.is_none() {
                        free_slot = Some((lba, offset));
                    }
                    continue;
                }
                if first == ENTRY_DOT || is_skippable_attr(chunk[11]) {
                    continue;
                }
                if chunk[0..11] == target[..] {
                    return Ok((lba, offset, Some(decode_entry(chunk))));
                }
            }
        }

        // Ran off the end of this directory's allocated space without
        // finding a free slot or the name. Root can't grow; a
        // subdirectory can, by chaining on a fresh cluster.
        match location {
            DirLocation::Root => {
                return match free_slot {
                    Some((lba, off)) => Ok((lba, off, None)),
                    None => Err("root directory is full"),
                };
            }
            DirLocation::Cluster(_) => {
                if let Some((lba, off)) = free_slot {
                    return Ok((lba, off, None));
                }
                let last = cluster.expect("Cluster(_) location always has Some(cluster) here");
                let new_cluster = allocate_cluster(l)?;
                set_fat_entry(l, last, new_cluster as u16)?;
                zero_cluster(l, new_cluster)?;
                cluster = Some(new_cluster);
                // Loop again: the freshly zeroed cluster's first entry
                // (offset 0, first byte 0x00) will be picked up as the
                // free slot on the next pass.
            }
        }
    }
}

/// Creates a file at `path` with contents `data`, overwriting it (and
/// freeing its old cluster chain) if it already exists. Directories in
/// `path` must already exist -- this doesn't create them.
pub fn write_file(path: &str, data: &[u8]) -> Result<(), &'static str> {
    let l = layout()?;
    let (dir_part, filename) = split_path(path);
    if filename.is_empty() {
        return Err("not a file");
    }
    let dir_location = resolve_dir(dir_part)?;
    let target = to_short_name(filename);

    let (lba, offset, existing) = locate_slot(l, dir_location, &target)?;

    if let Some(existing) = &existing {
        if existing.is_dir {
            return Err("is a directory");
        }
        if existing.cluster >= 2 {
            free_chain(l, existing.cluster)?;
        }
    }

    // Allocate and write the new cluster chain, linking each cluster to
    // the next as we go.
    let mut first_cluster: u32 = 0;
    let mut prev_cluster: Option<u32> = None;
    let cluster_bytes = (l.sectors_per_cluster * l.bytes_per_sector) as usize;
    let mut offset_in_data = 0usize;

    while offset_in_data < data.len() {
        let cluster = allocate_cluster(l)?;
        if first_cluster == 0 {
            first_cluster = cluster;
        }
        if let Some(prev) = prev_cluster {
            set_fat_entry(l, prev, cluster as u16)?;
        }
        for s in 0..l.sectors_per_cluster {
            let mut sector_buf = [0u8; SECTOR_SIZE];
            let start = offset_in_data + s as usize * l.bytes_per_sector as usize;
            if start < data.len() {
                let end = core::cmp::min(start + l.bytes_per_sector as usize, data.len());
                sector_buf[..end - start].copy_from_slice(&data[start..end]);
            }
            write_sector(cluster_to_lba(l, cluster) + s, &sector_buf)?;
        }
        offset_in_data += cluster_bytes;
        prev_cluster = Some(cluster);
    }
    // allocate_cluster already leaves a freshly claimed cluster marked
    // end-of-chain, so the last cluster in the loop above is already
    // correctly terminated -- nothing further to do for an empty file
    // either, since first_cluster just stays 0 (FAT's own "no data" case).

    let mut entry = [0u8; DIR_ENTRY_SIZE];
    entry[0..11].copy_from_slice(&target);
    entry[11] = 0; // Attributes: a plain file.
    entry[26] = (first_cluster & 0xFF) as u8;
    entry[27] = ((first_cluster >> 8) & 0xFF) as u8;
    entry[28..32].copy_from_slice(&(data.len() as u32).to_le_bytes());

    let mut sector_buf = read_sector(lba)?;
    sector_buf[offset..offset + DIR_ENTRY_SIZE].copy_from_slice(&entry);
    write_sector(lba, &sector_buf)?;

    Ok(())
}

/// Deletes a file at `path`: frees its cluster chain and marks its
/// directory entry deleted. Directories aren't supported (there's no
/// `rmdir` here, deliberately -- removing a non-empty directory safely
/// needs recursion this driver doesn't have yet).
pub fn remove_file(path: &str) -> Result<(), &'static str> {
    let l = layout()?;
    let (dir_part, filename) = split_path(path);
    if filename.is_empty() {
        return Err("not a file");
    }
    let dir_location = resolve_dir(dir_part)?;
    let target = to_short_name(filename);

    let (lba, offset, existing) = locate_slot(l, dir_location, &target)?;
    let existing = existing.ok_or("no such file")?;
    if existing.is_dir {
        return Err("is a directory (rmdir not supported)");
    }
    if existing.cluster >= 2 {
        free_chain(l, existing.cluster)?;
    }

    let mut buf = read_sector(lba)?;
    buf[offset] = ENTRY_DELETED;
    write_sector(lba, &buf)?;

    Ok(())
}

#[allow(dead_code)] // Handy for future diagnostics; not wired to a command yet.
pub fn sectors_read() -> u64 {
    SECTORS_READ.load(Ordering::Relaxed)
}

#[allow(dead_code)] // Handy for future diagnostics; not wired to a command yet.
pub fn sectors_written() -> u64 {
    SECTORS_WRITTEN.load(Ordering::Relaxed)
}
