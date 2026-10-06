//! KonjacFS, read-only for now (milestone 1 of docs/kfs-design.md).
//!
//! Mounting picks the newest valid superblock of the eight in the ring.
//! From there everything is reached through block pointers, and every
//! read through one is checked against the xxHash64 the pointer carries
//! -- a damaged block is an error, never wrong data.
//!
//! All metadata lives in one B+tree whose items are keyed by
//! `(object, kind, offset)`, so a file's inode, its directory entries or
//! extents, all sit together in key order. Tree nodes go through a small
//! fixed-size cache ([`CACHE_SLOTS`] x 4 KiB); file data is read straight
//! from the disk into the caller's buffer.

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::block;
use crate::sync::IrqSpinLock;

pub const BLOCK: usize = 4096;
const SECTORS_PER_BLOCK: u64 = (BLOCK / block::SECTOR_SIZE) as u64;
const SB_FIRST: u64 = 1;
const SB_SLOTS: u64 = 8;
const MAGIC_SB: &[u8; 8] = b"KONJACFS";
const MAGIC_NODE: &[u8; 4] = b"KFSN";
const VERSION: u32 = 1;

const TREE_FS: u8 = 1;
const HEADER: usize = 64;
const KEY: usize = 24;
const BP: usize = 32;
const ITEM_HEADER: usize = 24;
const INTERIOR_ENTRY: usize = KEY + BP;

const K_INODE: u8 = 1;
const K_DIR_ENTRY: u8 = 2;
const K_INLINE: u8 = 3;
const K_EXTENT: u8 = 4;
const T_DIR: u8 = 2;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const ROOT_OBJECT: u64 = 1;
/// The longest an extent can be: one checksum's worth.
const EXTENT_MAX: u64 = 64 * 1024;

/// Tree nodes kept in memory: 256 x 4 KiB = 1 MiB.
const CACHE_SLOTS: usize = 256;

// --- xxHash64 -------------------------------------------------------------------

const P1: u64 = 11400714785074694791;
const P2: u64 = 14029467366897019727;
const P3: u64 = 1609587929392839161;
const P4: u64 = 9650029242287828579;
const P5: u64 = 2870177450012600261;

fn round(acc: u64, lane: u64) -> u64 {
    acc.wrapping_add(lane.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
}

fn merge(acc: u64, v: u64) -> u64 {
    (acc ^ round(0, v)).wrapping_mul(P1).wrapping_add(P4)
}

fn le64(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

fn le32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn le16(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes(b[i..i + 2].try_into().unwrap())
}

/// xxHash64 with seed 0.
pub fn xxh64(data: &[u8]) -> u64 {
    let n = data.len();
    let mut i = 0;
    let mut h;
    if n >= 32 {
        let (mut v1, mut v2, mut v3, mut v4) = (P1.wrapping_add(P2), P2, 0u64, 0u64.wrapping_sub(P1));
        while i + 32 <= n {
            v1 = round(v1, le64(data, i));
            v2 = round(v2, le64(data, i + 8));
            v3 = round(v3, le64(data, i + 16));
            v4 = round(v4, le64(data, i + 24));
            i += 32;
        }
        h = v1.rotate_left(1).wrapping_add(v2.rotate_left(7)).wrapping_add(v3.rotate_left(12)).wrapping_add(v4.rotate_left(18));
        for v in [v1, v2, v3, v4] {
            h = merge(h, v);
        }
    } else {
        h = P5;
    }
    h = h.wrapping_add(n as u64);
    while i + 8 <= n {
        h ^= round(0, le64(data, i));
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        i += 8;
    }
    if i + 4 <= n {
        h ^= (le32(data, i) as u64).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        i += 4;
    }
    while i < n {
        h ^= (data[i] as u64).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
        i += 1;
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

// --- On-disk structures ---------------------------------------------------------

/// A block pointer: where some blocks are, and what they must hash to.
#[derive(Clone, Copy, Debug)]
struct Bp {
    block: u64,
    count: u32,
    checksum: u64,
}

impl Bp {
    fn parse(b: &[u8], off: usize) -> Bp {
        Bp { block: le64(b, off), count: le32(b, off + 8), checksum: le64(b, off + 24) }
    }
}

/// `(object, kind, offset)`, compared in that order.
type Key = (u64, u8, u64);

fn parse_key(b: &[u8], off: usize) -> Key {
    (le64(b, off), b[off + 16], le64(b, off + 8))
}

struct Mount {
    disk: usize,
    total_blocks: u64,
    free_blocks: u64,
    txg: u64,
    fs_root: Bp,
    label: String,
}

/// The tree-node cache: least recently used slot goes first.
struct Cache {
    slots: Vec<(u64, u64, u64, Box<[u8; BLOCK]>)>, // block, checksum, last use, contents
    clock: u64,
}

static MOUNT: IrqSpinLock<Option<Mount>> = IrqSpinLock::new(None);
static CACHE: IrqSpinLock<Cache> = IrqSpinLock::new(Cache { slots: Vec::new(), clock: 0 });

fn mount_info() -> Result<(usize, Bp), &'static str> {
    MOUNT.lock().as_ref().map(|m| (m.disk, m.fs_root)).ok_or("kfs: no KFS disk")
}

fn read_blocks(disk: usize, block: u64, out: &mut [u8]) -> Result<(), &'static str> {
    block::read(disk, block * SECTORS_PER_BLOCK, out)
}

/// Looks for a KFS volume on every disk and mounts the first one found.
pub fn mount() -> Result<(), &'static str> {
    let mut buf = alloc::vec![0u8; BLOCK];
    for disk in 0..block::count() {
        let mut best: Option<Mount> = None;
        for slot in 0..SB_SLOTS {
            if read_blocks(disk, SB_FIRST + slot, &mut buf).is_err() {
                break;
            }
            if &buf[0..8] != MAGIC_SB || le64(&buf, BLOCK - 8) != xxh64(&buf[..BLOCK - 8]) {
                continue;
            }
            if le32(&buf, 8) != VERSION || le32(&buf, 12) as usize != BLOCK {
                continue;
            }
            let txg = le64(&buf, 24);
            if best.as_ref().is_some_and(|b| b.txg >= txg) {
                continue;
            }
            let label_end = buf[168..200].iter().position(|&c| c == 0).unwrap_or(32);
            best = Some(Mount {
                disk,
                total_blocks: le64(&buf, 16),
                free_blocks: le64(&buf, 136),
                txg,
                fs_root: Bp::parse(&buf, 40),
                label: String::from(core::str::from_utf8(&buf[168..168 + label_end]).unwrap_or("?")),
            });
        }
        if let Some(m) = best {
            if block::capacity(disk).is_some_and(|sectors| m.total_blocks * SECTORS_PER_BLOCK > sectors) {
                return Err("the KFS volume is larger than its disk (damaged, or the image was truncated)");
            }
            *MOUNT.lock() = Some(m);
            let mut c = CACHE.lock();
            c.slots.clear();
            c.slots.reserve_exact(CACHE_SLOTS);
            return Ok(());
        }
    }
    Err("no KFS volume on any disk")
}

pub fn mounted() -> bool {
    MOUNT.lock().is_some()
}

/// `(label, txg, total blocks, free blocks)` of the mounted volume.
pub fn info() -> Option<(String, u64, u64, u64)> {
    MOUNT.lock().as_ref().map(|m| (m.label.clone(), m.txg, m.total_blocks, m.free_blocks))
}

/// A tree node, from the cache or the disk, checksum verified.
fn node(disk: usize, bp: Bp) -> Result<Box<[u8; BLOCK]>, &'static str> {
    {
        let mut c = CACHE.lock();
        c.clock += 1;
        let now = c.clock;
        if let Some(slot) = c.slots.iter_mut().find(|s| s.0 == bp.block && s.1 == bp.checksum) {
            slot.2 = now;
            return Ok(slot.3.clone());
        }
    }
    let mut b: Box<[u8; BLOCK]> = Box::new([0u8; BLOCK]);
    read_blocks(disk, bp.block, &mut b[..])?;
    if xxh64(&b[..]) != bp.checksum {
        return Err("kfs: checksum mismatch in a tree node (the disk is damaged)");
    }
    if &b[0..4] != MAGIC_NODE || b[5] != TREE_FS {
        return Err("kfs: a tree pointer leads to something that isn't a tree node");
    }
    let mut c = CACHE.lock();
    let now = c.clock;
    if c.slots.len() < CACHE_SLOTS {
        c.slots.push((bp.block, bp.checksum, now, b.clone()));
    } else if let Some(oldest) = c.slots.iter_mut().min_by_key(|s| s.2) {
        *oldest = (bp.block, bp.checksum, now, b.clone());
    }
    Ok(b)
}

/// Calls `f` on every item with a key `>= start`, in key order, until it
/// returns `false`.
fn scan(start: Key, f: &mut dyn FnMut(Key, &[u8]) -> bool) -> Result<(), &'static str> {
    let (disk, root) = mount_info()?;
    scan_node(disk, root, start, f, 0).map(|_| ())
}

fn scan_node(disk: usize, bp: Bp, start: Key, f: &mut dyn FnMut(Key, &[u8]) -> bool, depth: u32) -> Result<bool, &'static str> {
    if depth > 16 {
        return Err("kfs: the tree is impossibly deep (damaged)");
    }
    let n = node(disk, bp)?;
    let level = n[4];
    let count = le16(&n[..], 6) as usize;
    if level == 0 {
        if HEADER + count * ITEM_HEADER > BLOCK {
            return Err("kfs: damaged leaf");
        }
        for i in 0..count {
            let h = HEADER + i * ITEM_HEADER;
            let key = parse_key(&n[..], h);
            if key < start {
                continue;
            }
            let (off, len) = (le16(&n[..], h + 20) as usize, le16(&n[..], h + 22) as usize);
            if off + len > BLOCK {
                return Err("kfs: damaged leaf");
            }
            if !f(key, &n[off..off + len]) {
                return Ok(false);
            }
        }
        Ok(true)
    } else {
        if HEADER + count * INTERIOR_ENTRY > BLOCK {
            return Err("kfs: damaged tree node");
        }
        // Start at the last child whose first key is <= start.
        let mut first = 0;
        for i in 0..count {
            if parse_key(&n[..], HEADER + i * INTERIOR_ENTRY) <= start {
                first = i;
            } else {
                break;
            }
        }
        for i in first..count {
            let child = Bp::parse(&n[..], HEADER + i * INTERIOR_ENTRY + KEY);
            if !scan_node(disk, child, start, f, depth + 1)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// An object's inode: `(is_dir, size)`.
fn inode(obj: u64) -> Result<(bool, u64), &'static str> {
    let mut found = None;
    scan((obj, K_INODE, 0), &mut |k, d| {
        if k == (obj, K_INODE, 0) && d.len() >= 16 {
            found = Some((le32(d, 0) & S_IFMT == S_IFDIR, le64(d, 8)));
        }
        false
    })?;
    found.ok_or("kfs: missing inode (damaged)")
}

/// The directory entry's key offset for `name`: see `name_hash` in
/// tools/kfs.py.
fn name_hash(name: &[u8]) -> u64 {
    xxh64(name) & !7
}

/// The child called `name` in directory `dir`: `(object, is_dir)`.
fn lookup(dir: u64, name: &str) -> Result<Option<(u64, bool)>, &'static str> {
    let base = name_hash(name.as_bytes());
    let mut found = None;
    scan((dir, K_DIR_ENTRY, base), &mut |k, d| {
        if k.0 != dir || k.1 != K_DIR_ENTRY || k.2 >= base + 8 {
            return false;
        }
        if d.len() >= 10 && d.get(10..10 + d[9] as usize) == Some(name.as_bytes()) {
            found = Some((le64(d, 0), d[8] == T_DIR));
            return false;
        }
        true
    })?;
    Ok(found)
}

/// The object at absolute path `path` (relative to the volume's root).
fn resolve(path: &str) -> Result<(u64, bool), &'static str> {
    let mut cur = (ROOT_OBJECT, true);
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if !cur.1 {
            return Err("not a directory");
        }
        cur = lookup(cur.0, part)?.ok_or("no such file or directory")?;
    }
    Ok(cur)
}

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

pub fn list_dir(path: &str) -> Result<Vec<Entry>, &'static str> {
    let (dir, is_dir) = resolve(path)?;
    if !is_dir {
        return Err("not a directory");
    }
    let mut kids: Vec<(String, u64, bool)> = Vec::new();
    scan((dir, K_DIR_ENTRY, 0), &mut |k, d| {
        if k.0 != dir || k.1 != K_DIR_ENTRY {
            return false;
        }
        if d.len() >= 10 {
            let name = d.get(10..10 + d[9] as usize).unwrap_or(&[]);
            kids.push((String::from(core::str::from_utf8(name).unwrap_or("?")), le64(d, 0), d[8] == T_DIR));
        }
        true
    })?;
    let mut out = Vec::with_capacity(kids.len());
    for (name, obj, is_dir) in kids {
        let size = if is_dir { 0 } else { inode(obj)?.1 };
        out.push(Entry { name, is_dir, size });
    }
    Ok(out)
}

/// `(is_dir, size)` of whatever `path` names.
pub fn stat(path: &str) -> Result<(bool, u64), &'static str> {
    let (obj, _) = resolve(path)?;
    inode(obj)
}

/// One piece of a file: inline data, or an extent of whole blocks.
enum Piece {
    Inline(Vec<u8>),
    Extent { offset: u64, length: u64, bp: Bp },
}

/// The pieces of object `obj` that overlap byte range `from..to`.
fn pieces(obj: u64, from: u64, to: u64) -> Result<Vec<Piece>, &'static str> {
    let mut out = Vec::new();
    // Inline data comes before extents in key order; one search finds
    // either kind. An extent covering `from` starts at most EXTENT_MAX
    // bytes before it.
    let mut inline = None;
    scan((obj, K_INLINE, 0), &mut |k, d| {
        if k == (obj, K_INLINE, 0) {
            inline = Some(Vec::from(d));
        }
        false
    })?;
    if let Some(d) = inline {
        out.push(Piece::Inline(d));
        return Ok(out);
    }
    let start = from.saturating_sub(EXTENT_MAX - 1);
    scan((obj, K_EXTENT, start), &mut |k, d| {
        if k.0 != obj || k.1 != K_EXTENT || k.2 >= to {
            return false;
        }
        if d.len() >= BP + 8 {
            let length = le64(d, BP);
            if k.2 + length > from {
                out.push(Piece::Extent { offset: k.2, length, bp: Bp::parse(d, 0) });
            }
        }
        true
    })?;
    Ok(out)
}

/// Reads extent `bp` into `buf` (exactly `bp.count` blocks) and checks it.
fn read_extent(disk: usize, bp: Bp, buf: &mut [u8]) -> Result<(), &'static str> {
    if bp.count == 0 || bp.count as u64 * BLOCK as u64 > EXTENT_MAX {
        return Err("kfs: damaged extent");
    }
    read_blocks(disk, bp.block, buf)?;
    if xxh64(buf) != bp.checksum {
        return Err("kfs: checksum mismatch in file data (the disk is damaged)");
    }
    Ok(())
}

pub fn read_file(path: &str) -> Result<Vec<u8>, &'static str> {
    let (obj, is_dir) = resolve(path)?;
    if is_dir {
        return Err("is a directory");
    }
    let (_, size) = inode(obj)?;
    let disk = mount_info()?.0;
    // Room for whole blocks, so extents can be read straight in.
    let mut data = Vec::new();
    let rounded = (size as usize).div_ceil(BLOCK) * BLOCK;
    data.try_reserve_exact(rounded).map_err(|_| "file buffer allocation failed")?;
    data.resize(rounded.max(size as usize), 0);
    for p in pieces(obj, 0, size)? {
        match p {
            Piece::Inline(d) => {
                let n = d.len().min(size as usize);
                data[..n].copy_from_slice(&d[..n]);
            }
            Piece::Extent { offset, bp, .. } => {
                let at = offset as usize;
                let bytes = bp.count as usize * BLOCK;
                if at + bytes > data.len() {
                    return Err("kfs: an extent runs past the end of its file (damaged)");
                }
                read_extent(disk, bp, &mut data[at..at + bytes])?;
            }
        }
    }
    data.truncate(size as usize);
    Ok(data)
}

/// An open file: reads at any offset without reading the rest.
#[derive(Clone, Copy)]
pub struct File {
    obj: u64,
    pub size: u64,
}

pub fn open_file(path: &str) -> Result<File, &'static str> {
    let (obj, is_dir) = resolve(path)?;
    if is_dir {
        return Err("is a directory");
    }
    Ok(File { obj, size: inode(obj)?.1 })
}

impl File {
    /// Fills `out` from byte `offset`; returns how much was read (less at
    /// the end of the file). Unwritten ranges read as zeros.
    pub fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, &'static str> {
        if offset >= self.size || out.is_empty() {
            return Ok(0);
        }
        let want = out.len().min((self.size - offset) as usize);
        let end = offset + want as u64;
        out[..want].fill(0);
        let disk = mount_info()?.0;
        let mut buf: Vec<u8> = Vec::new();
        for p in pieces(self.obj, offset, end)? {
            let (start, bytes): (u64, &[u8]) = match &p {
                Piece::Inline(d) => (0, &d[..]),
                Piece::Extent { offset: at, length, bp } => {
                    buf.resize(bp.count as usize * BLOCK, 0);
                    read_extent(disk, *bp, &mut buf)?;
                    (*at, &buf[..(*length as usize).min(buf.len())])
                }
            };
            // The overlap of this piece with offset..end.
            let lo = start.max(offset);
            let hi = (start + bytes.len() as u64).min(end);
            if hi > lo {
                out[(lo - offset) as usize..(hi - offset) as usize].copy_from_slice(&bytes[(lo - start) as usize..(hi - start) as usize]);
            }
        }
        Ok(want)
    }
}
