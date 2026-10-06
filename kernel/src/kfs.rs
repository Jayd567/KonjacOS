//! KonjacFS (docs/kfs-design.md): reading and writing.
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
//!
//! Writing is copy-on-write. Changing the tree copies each node on the
//! path into memory, and every operation (saving a file, a rename, ...)
//! ends in a commit: the changed nodes are written bottom-up to free
//! blocks, then the changed parts of the free-space bitmap, a flush, the
//! next superblock in the ring, and another flush. File data goes to
//! newly allocated blocks before that. Blocks the old state still uses
//! are never written, and become free only once the commit has landed,
//! so until the superblock is written the disk still holds the previous
//! state complete: a crash loses at most the operation in progress.

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::block;
use crate::sync::SpinLock;

pub const BLOCK: usize = 4096;
const SECTORS_PER_BLOCK: u64 = (BLOCK / block::SECTOR_SIZE) as u64;
const SB_FIRST: u64 = 1;
const SB_SLOTS: u64 = 8;
const DATA_START: u64 = 16;
const MAGIC_SB: &[u8; 8] = b"KONJACFS";
const MAGIC_NODE: &[u8; 4] = b"KFSN";
const VERSION: u32 = 1;

const TREE_FS: u8 = 1;
const TREE_BITMAP: u8 = 2;
const BP_NODE: u8 = 1;
const BP_BITMAP_INDEX: u8 = 2;
const BP_BITMAP: u8 = 3;
const BP_DATA: u8 = 4;

const HEADER: usize = 64;
const KEY: usize = 24;
const BP: usize = 32;
const ITEM_HEADER: usize = 24;
const INTERIOR_ENTRY: usize = KEY + BP;
const INDEX_MAX: usize = (BLOCK - HEADER) / BP; // 126
const BITS_PER_BITMAP: u64 = BLOCK as u64 * 8; // 128 MiB of disk per bitmap block
const WORDS_PER_BITMAP: usize = BLOCK / 8;

const K_INODE: u8 = 1;
const K_DIR_ENTRY: u8 = 2;
const K_INLINE: u8 = 3;
const K_EXTENT: u8 = 4;
const T_FILE: u8 = 1;
const T_DIR: u8 = 2;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const ROOT_OBJECT: u64 = 1;
const INODE_SIZE: usize = 128;
const NAME_MAX: usize = 255;
/// Files up to this size live in an `INLINE` item next to their inode.
const INLINE_MAX: usize = 2048;
/// The longest an extent can be: one checksum's worth.
const EXTENT_MAX: u64 = 64 * 1024;
const EXTENT_BLOCKS: u64 = EXTENT_MAX / BLOCK as u64;

/// Tree nodes kept in memory: 256 x 4 KiB = 1 MiB.
const CACHE_SLOTS: usize = 256;
/// Copied tree nodes an operation may pile up before it commits part of
/// the way (only a big delete gets there).
const DIRTY_LIMIT: usize = CACHE_SLOTS / 2;
/// Blocks file data may not use, so a full disk still has room for the
/// tree and bitmap blocks a commit writes -- including the commit that
/// deletes files to make space. At least this many, or 1/256 of the disk.
const RESERVE_MIN: u64 = 64;

const DAMAGED_LEAF: &str = "kfs: damaged leaf";
const TOO_DEEP: &str = "kfs: the tree is impossibly deep (damaged)";

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

fn put64(b: &mut [u8], i: usize, v: u64) {
    b[i..i + 8].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], i: usize, v: u32) {
    b[i..i + 4].copy_from_slice(&v.to_le_bytes());
}

fn put16(b: &mut [u8], i: usize, v: u16) {
    b[i..i + 2].copy_from_slice(&v.to_le_bytes());
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
#[derive(Clone, Copy, Debug, Default)]
struct Bp {
    block: u64,
    count: u32,
    kind: u8,
    birth: u64,
    checksum: u64,
}

impl Bp {
    fn parse(b: &[u8], off: usize) -> Bp {
        Bp { block: le64(b, off), count: le32(b, off + 8), kind: b[off + 12], birth: le64(b, off + 16), checksum: le64(b, off + 24) }
    }

    fn put(&self, b: &mut [u8], off: usize) {
        put64(b, off, self.block);
        put32(b, off + 8, self.count);
        b[off + 12] = self.kind;
        b[off + 13..off + 16].fill(0);
        put64(b, off + 16, self.birth);
        put64(b, off + 24, self.checksum);
    }
}

/// `(object, kind, offset)`, compared in that order.
type Key = (u64, u8, u64);

fn parse_key(b: &[u8], off: usize) -> Key {
    (le64(b, off), b[off + 16], le64(b, off + 8))
}

fn put_key(b: &mut [u8], off: usize, k: Key) {
    put64(b, off, k.0);
    put64(b, off + 8, k.2);
    b[off + 16] = k.1;
    b[off + 17..off + KEY].fill(0);
}

#[derive(Clone, Copy, Default)]
struct Inode {
    mode: u32,
    flags: u32,
    size: u64,
    links: u32,
    generation: u32,
    created: u64,
    modified: u64,
    changed: u64,
    accessed: u64,
    blocks: u64,
}

impl Inode {
    fn new(mode: u32, size: u64, links: u32, now: u64, blocks: u64) -> Inode {
        Inode { mode, size, links, created: now, modified: now, changed: now, accessed: now, blocks, ..Inode::default() }
    }

    fn parse(d: &[u8]) -> Result<Inode, &'static str> {
        if d.len() < 64 {
            return Err("kfs: damaged inode");
        }
        Ok(Inode {
            mode: le32(d, 0),
            flags: le32(d, 4),
            size: le64(d, 8),
            links: le32(d, 16),
            generation: le32(d, 20),
            created: le64(d, 24),
            modified: le64(d, 32),
            changed: le64(d, 40),
            accessed: le64(d, 48),
            blocks: le64(d, 56),
        })
    }

    fn bytes(&self) -> Vec<u8> {
        let mut d = vec![0u8; INODE_SIZE];
        put32(&mut d, 0, self.mode);
        put32(&mut d, 4, self.flags);
        put64(&mut d, 8, self.size);
        put32(&mut d, 16, self.links);
        put32(&mut d, 20, self.generation);
        put64(&mut d, 24, self.created);
        put64(&mut d, 32, self.modified);
        put64(&mut d, 40, self.changed);
        put64(&mut d, 48, self.accessed);
        put64(&mut d, 56, self.blocks);
        d
    }

    fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
}

/// The directory entry's key offset for `name`: see `name_hash` in
/// tools/kfs.py.
fn name_hash(name: &[u8]) -> u64 {
    xxh64(name) & !7
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= NAME_MAX && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// `"/a/b/c"` -> `("/a/b", "c")`.
fn split_path(path: &str) -> Result<(&str, &str), &'static str> {
    let p = path.trim_end_matches('/');
    let cut = p.rfind('/').ok_or("not a file or folder name")?;
    let name = &p[cut + 1..];
    if name.is_empty() {
        return Err("not a file or folder name");
    }
    Ok((&p[..cut], name))
}

fn is_root(path: &str) -> bool {
    path.trim_matches('/').is_empty()
}

fn now_ns() -> u64 {
    crate::rtc::unix_seconds().saturating_mul(1_000_000_000)
}

fn read_blocks(disk: usize, block: u64, out: &mut [u8]) -> Result<(), &'static str> {
    block::read(disk, block * SECTORS_PER_BLOCK, out)
}

fn write_blocks(disk: usize, block: u64, data: &[u8]) -> Result<(), &'static str> {
    block::write(disk, block * SECTORS_PER_BLOCK, data)
}

// --- Tree nodes in memory ---------------------------------------------------------

/// A child pointer: a node on disk, or one copied into memory (an index
/// into `Fs::nodes`) because the operation in progress changed it.
#[derive(Clone, Copy)]
enum Child {
    Disk(Bp),
    Mem(usize),
}

/// A tree node being changed. A leaf has `items`; an interior node has
/// `kids`, each with the smallest key that can be under it.
#[derive(Default)]
struct Node {
    level: u8,
    items: Vec<(Key, Vec<u8>)>,
    kids: Vec<(Key, Child)>,
}

impl Node {
    fn leaf(items: Vec<(Key, Vec<u8>)>) -> Node {
        Node { level: 0, items, kids: Vec::new() }
    }

    fn parse(n: &[u8; BLOCK]) -> Result<Node, &'static str> {
        let level = n[4];
        let count = le16(&n[..], 6) as usize;
        let mut out = Node { level, ..Node::default() };
        if level == 0 {
            if HEADER + count * ITEM_HEADER > BLOCK {
                return Err(DAMAGED_LEAF);
            }
            for i in 0..count {
                let h = HEADER + i * ITEM_HEADER;
                let (off, len) = (le16(&n[..], h + 20) as usize, le16(&n[..], h + 22) as usize);
                if off + len > BLOCK {
                    return Err(DAMAGED_LEAF);
                }
                out.items.push((parse_key(&n[..], h), Vec::from(&n[off..off + len])));
            }
        } else {
            if HEADER + count * INTERIOR_ENTRY > BLOCK {
                return Err("kfs: damaged tree node");
            }
            for i in 0..count {
                let e = HEADER + i * INTERIOR_ENTRY;
                out.kids.push((parse_key(&n[..], e), Child::Disk(Bp::parse(&n[..], e + KEY))));
            }
        }
        Ok(out)
    }

    /// The node as a block. Every child must already be on disk.
    fn serialize(&self, txg: u64) -> Box<[u8; BLOCK]> {
        let mut b: Box<[u8; BLOCK]> = Box::new([0u8; BLOCK]);
        b[0..4].copy_from_slice(MAGIC_NODE);
        b[4] = self.level;
        b[5] = TREE_FS;
        put64(&mut b[..], 8, txg);
        if self.level == 0 {
            put16(&mut b[..], 6, self.items.len() as u16);
            let mut end = BLOCK;
            for (i, (k, d)) in self.items.iter().enumerate() {
                end -= d.len();
                b[end..end + d.len()].copy_from_slice(d);
                let h = HEADER + i * ITEM_HEADER;
                put_key(&mut b[..], h, *k);
                put16(&mut b[..], h + 20, end as u16);
                put16(&mut b[..], h + 22, d.len() as u16);
            }
        } else {
            put16(&mut b[..], 6, self.kids.len() as u16);
            for (i, (k, c)) in self.kids.iter().enumerate() {
                let e = HEADER + i * INTERIOR_ENTRY;
                put_key(&mut b[..], e, *k);
                if let Child::Disk(bp) = c {
                    bp.put(&mut b[..], e + KEY);
                }
            }
        }
        b
    }

    /// Bytes this node needs as a block (more than [`BLOCK`]: it must split).
    fn size(&self) -> usize {
        if self.level == 0 {
            HEADER + self.items.iter().map(|(_, d)| ITEM_HEADER + d.len()).sum::<usize>()
        } else {
            HEADER + self.kids.len() * INTERIOR_ENTRY
        }
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty() && self.kids.is_empty()
    }

    fn first_key(&self) -> Option<Key> {
        if self.level == 0 { self.items.first().map(|i| i.0) } else { self.kids.first().map(|k| k.0) }
    }
}

/// The child of an interior node that `key` belongs under.
fn child_index(kids: &[(Key, Child)], key: Key) -> usize {
    kids.partition_point(|k| k.0 <= key).saturating_sub(1)
}

// --- The mounted volume ---------------------------------------------------------------

/// The tree-node cache: least recently used slot goes first.
struct Cache {
    slots: Vec<(u64, u64, u64, Box<[u8; BLOCK]>)>, // block, checksum, last use, contents
    clock: u64,
}

impl Cache {
    fn get(&mut self, bp: Bp) -> Option<Box<[u8; BLOCK]>> {
        self.clock += 1;
        let now = self.clock;
        let slot = self.slots.iter_mut().find(|s| s.0 == bp.block && s.1 == bp.checksum)?;
        slot.2 = now;
        Some(slot.3.clone())
    }

    fn put(&mut self, block: u64, checksum: u64, data: &[u8; BLOCK]) {
        let now = self.clock;
        if self.slots.len() < CACHE_SLOTS {
            self.slots.push((block, checksum, now, Box::new(*data)));
        } else if let Some(oldest) = self.slots.iter_mut().min_by_key(|s| s.2) {
            oldest.0 = block;
            oldest.1 = checksum;
            oldest.2 = now;
            oldest.3.copy_from_slice(data);
        }
    }
}

struct Fs {
    disk: usize,
    /// The newest committed superblock; the next one starts as a copy.
    sb: Box<[u8; BLOCK]>,
    total_blocks: u64,
    txg: u64,
    label: String,
    next_object: u64,
    cache: Cache,

    // Free space. The whole bitmap is in memory (32 KiB per GiB of disk).
    /// 1 = in use, in the state being built.
    bits: Vec<u64>,
    /// Freed since the last commit, so still in use by the committed
    /// state: not to be handed out again until the next commit lands.
    held: Vec<u64>,
    /// Where each bitmap block is, and which have changed.
    bitmap: Vec<Bp>,
    bitmap_dirty: Vec<bool>,
    /// The bitmap index's nodes, to free when it's rewritten.
    index: Vec<Bp>,
    /// Allocation goes forward from here, so new data lands in one run.
    cursor: u64,

    // The operation in progress.
    root: Child,
    nodes: Vec<Node>,
    dirty: bool,
}

static FS: SpinLock<Option<Fs>> = SpinLock::new(None);

/// Runs `f` on the mounted volume. One operation at a time: a reader
/// never sees an operation half done.
fn with<T>(f: impl FnOnce(&mut Fs) -> Result<T, &'static str>) -> Result<T, &'static str> {
    let mut guard = FS.lock();
    f(guard.as_mut().ok_or("kfs: no KFS disk")?)
}

/// Runs `f`, then commits what it changed. If anything fails, the
/// changes are dropped and the volume is back at its last commit.
fn change<T>(f: impl FnOnce(&mut Fs) -> Result<T, &'static str>) -> Result<T, &'static str> {
    let mut guard = FS.lock();
    let fs = guard.as_mut().ok_or("kfs: no KFS disk")?;
    let result = f(fs).and_then(|v| fs.commit().map(|_| v));
    if result.is_err() && fs.abort().is_err() {
        // Not even the committed state reads back: stop using the disk.
        *guard = None;
    }
    result
}

/// Looks for a KFS volume on every disk and mounts the first one found.
pub fn mount() -> Result<(), &'static str> {
    let mut buf: Box<[u8; BLOCK]> = Box::new([0u8; BLOCK]);
    for disk in 0..block::count() {
        let mut best: Option<Box<[u8; BLOCK]>> = None;
        for slot in 0..SB_SLOTS {
            if read_blocks(disk, SB_FIRST + slot, &mut buf[..]).is_err() {
                break;
            }
            if &buf[0..8] != MAGIC_SB || le64(&buf[..], BLOCK - 8) != xxh64(&buf[..BLOCK - 8]) {
                continue;
            }
            if le32(&buf[..], 8) != VERSION || le32(&buf[..], 12) as usize != BLOCK {
                continue;
            }
            if best.as_ref().is_some_and(|b| le64(&b[..], 24) >= le64(&buf[..], 24)) {
                continue;
            }
            best = Some(buf.clone());
        }
        let Some(sb) = best else { continue };
        let total_blocks = le64(&sb[..], 16);
        if block::capacity(disk).is_some_and(|sectors| total_blocks * SECTORS_PER_BLOCK > sectors) {
            return Err("the KFS volume is larger than its disk (damaged, or the image was truncated)");
        }
        let label_end = sb[168..200].iter().position(|&c| c == 0).unwrap_or(32);
        let mut fs = Fs {
            disk,
            total_blocks,
            txg: le64(&sb[..], 24),
            label: String::from(core::str::from_utf8(&sb[168..168 + label_end]).unwrap_or("?")),
            next_object: le64(&sb[..], 144),
            cache: Cache { slots: Vec::with_capacity(CACHE_SLOTS), clock: 0 },
            bits: Vec::new(),
            held: Vec::new(),
            bitmap: Vec::new(),
            bitmap_dirty: Vec::new(),
            index: Vec::new(),
            cursor: DATA_START,
            root: Child::Disk(Bp::parse(&sb[..], 40)),
            nodes: Vec::new(),
            dirty: false,
            sb,
        };
        fs.load_bitmap()?;
        *FS.lock() = Some(fs);
        return Ok(());
    }
    Err("no KFS volume on any disk")
}

pub fn mounted() -> bool {
    FS.lock().is_some()
}

/// `(label, txg, total blocks, free blocks)` of the mounted volume.
pub fn info() -> Option<(String, u64, u64, u64)> {
    FS.lock().as_ref().map(|fs| (fs.label.clone(), fs.txg, fs.total_blocks, le64(&fs.sb[..], 136)))
}

impl Fs {
    // --- Free space ---

    /// Reads the committed bitmap into memory.
    fn load_bitmap(&mut self) -> Result<(), &'static str> {
        let n = self.total_blocks.div_ceil(BITS_PER_BITMAP) as usize;
        self.bitmap.clear();
        self.index.clear();
        self.walk_index(Bp::parse(&self.sb[..], 72), 0)?;
        if self.bitmap.len() != n {
            return Err("kfs: the free-space bitmap is the wrong size (damaged)");
        }
        self.bits = vec![0; n * WORDS_PER_BITMAP];
        self.held = vec![0; n * WORDS_PER_BITMAP];
        self.bitmap_dirty = vec![false; n];
        let mut buf: Box<[u8; BLOCK]> = Box::new([0u8; BLOCK]);
        for k in 0..n {
            read_blocks(self.disk, self.bitmap[k].block, &mut buf[..])?;
            if xxh64(&buf[..]) != self.bitmap[k].checksum {
                return Err("kfs: checksum mismatch in the free-space bitmap (the disk is damaged)");
            }
            for w in 0..WORDS_PER_BITMAP {
                self.bits[k * WORDS_PER_BITMAP + w] = le64(&buf[..], w * 8);
            }
        }
        Ok(())
    }

    fn walk_index(&mut self, bp: Bp, depth: u32) -> Result<(), &'static str> {
        if depth > 8 {
            return Err(TOO_DEEP);
        }
        let mut b: Box<[u8; BLOCK]> = Box::new([0u8; BLOCK]);
        read_blocks(self.disk, bp.block, &mut b[..])?;
        if xxh64(&b[..]) != bp.checksum {
            return Err("kfs: checksum mismatch in the free-space index (the disk is damaged)");
        }
        let (level, count) = (b[4], le16(&b[..], 6) as usize);
        if &b[0..4] != MAGIC_NODE || b[5] != TREE_BITMAP || level == 0 || count > INDEX_MAX {
            return Err("kfs: damaged free-space index");
        }
        self.index.push(bp);
        for i in 0..count {
            let child = Bp::parse(&b[..], HEADER + i * BP);
            if level == 1 {
                self.bitmap.push(child);
            } else {
                self.walk_index(child, depth + 1)?;
            }
        }
        Ok(())
    }

    fn is_free(&self, b: u64) -> bool {
        let (w, bit) = ((b / 64) as usize, 1u64 << (b % 64));
        (self.bits[w] | self.held[w]) & bit == 0
    }

    fn set_used(&mut self, start: u64, count: u64) {
        for b in start..start + count {
            self.bits[(b / 64) as usize] |= 1 << (b % 64);
            self.bitmap_dirty[(b / BITS_PER_BITMAP) as usize] = true;
        }
        self.cursor = start + count;
    }

    /// Finds `want` free blocks in a row, looking forward from the cursor
    /// and then wrapping round; failing that, the longest free run there
    /// is. Returns `(start, length)`.
    fn find_run(&self, want: u64) -> Option<(u64, u64)> {
        let mut best: Option<(u64, u64)> = None;
        let cursor = self.cursor.clamp(DATA_START, self.total_blocks);
        for (lo, hi) in [(cursor, self.total_blocks), (DATA_START, cursor)] {
            let (mut start, mut len) = (lo, 0);
            let mut b = lo;
            while b < hi {
                let w = (b / 64) as usize;
                if b % 64 == 0 && b + 64 <= hi && self.bits[w] | self.held[w] == u64::MAX {
                    len = 0;
                    b += 64;
                    continue;
                }
                if self.is_free(b) {
                    if len == 0 {
                        start = b;
                    }
                    len += 1;
                    if len == want {
                        return Some((start, len));
                    }
                    if best.is_none_or(|(_, l)| len > l) {
                        best = Some((start, len));
                    }
                } else {
                    len = 0;
                }
                b += 1;
            }
        }
        best
    }

    /// Exactly `count` free blocks in a row, marked used.
    fn alloc(&mut self, count: u64) -> Result<u64, &'static str> {
        match self.find_run(count) {
            Some((start, len)) if len == count => {
                self.set_used(start, count);
                Ok(start)
            }
            _ => Err("the KFS disk is full"),
        }
    }

    /// Frees what `bp` points at. Blocks the committed state uses stay
    /// held until the next commit lands; blocks written since then are
    /// reusable straight away.
    fn free(&mut self, bp: Bp) {
        let deferred = bp.birth <= self.txg;
        for b in bp.block..bp.block.saturating_add(bp.count as u64) {
            if b < DATA_START || b >= self.total_blocks {
                break; // A damaged pointer; never free the superblocks.
            }
            let (w, bit) = ((b / 64) as usize, 1u64 << (b % 64));
            self.bits[w] &= !bit;
            if deferred {
                self.held[w] |= bit;
            }
            self.bitmap_dirty[(b / BITS_PER_BITMAP) as usize] = true;
        }
    }

    /// Free blocks not held, and so available to this operation.
    fn available(&self) -> u64 {
        let used: u64 = self.bits.iter().zip(&self.held).map(|(b, h)| (b | h).count_ones() as u64).sum();
        self.total_blocks - (used - (self.bits.len() as u64 * 64 - self.total_blocks))
    }

    fn free_blocks(&self) -> u64 {
        let used: u64 = self.bits.iter().map(|w| w.count_ones() as u64).sum();
        // Bits past the end of the volume are set but aren't blocks.
        let past_end = self.bits.len() as u64 * 64 - self.total_blocks;
        self.total_blocks - (used - past_end)
    }

    // --- Tree ---

    /// A tree node, from the cache or the disk, checksum verified.
    fn node(&mut self, bp: Bp) -> Result<Box<[u8; BLOCK]>, &'static str> {
        if let Some(b) = self.cache.get(bp) {
            return Ok(b);
        }
        let mut b: Box<[u8; BLOCK]> = Box::new([0u8; BLOCK]);
        read_blocks(self.disk, bp.block, &mut b[..])?;
        if xxh64(&b[..]) != bp.checksum {
            return Err("kfs: checksum mismatch in a tree node (the disk is damaged)");
        }
        if &b[0..4] != MAGIC_NODE || b[5] != TREE_FS {
            return Err("kfs: a tree pointer leads to something that isn't a tree node");
        }
        self.cache.put(bp.block, bp.checksum, &b);
        Ok(b)
    }

    /// Calls `f` on every item with a key `>= start`, in key order, until
    /// it returns `false`. Sees the operation in progress's changes.
    fn scan(&mut self, start: Key, f: &mut dyn FnMut(Key, &[u8]) -> bool) -> Result<(), &'static str> {
        let root = self.root;
        self.scan_child(root, start, f, 0).map(|_| ())
    }

    fn scan_child(&mut self, c: Child, start: Key, f: &mut dyn FnMut(Key, &[u8]) -> bool, depth: u32) -> Result<bool, &'static str> {
        if depth > 16 {
            return Err(TOO_DEEP);
        }
        let kids: Vec<Child> = match c {
            Child::Mem(i) => {
                let n = &self.nodes[i];
                if n.level == 0 {
                    for (k, d) in &n.items {
                        if *k >= start && !f(*k, d) {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                n.kids[child_index(&n.kids, start)..].iter().map(|k| k.1).collect()
            }
            Child::Disk(bp) => {
                let n = self.node(bp)?;
                let count = le16(&n[..], 6) as usize;
                if n[4] == 0 {
                    if HEADER + count * ITEM_HEADER > BLOCK {
                        return Err(DAMAGED_LEAF);
                    }
                    for i in 0..count {
                        let h = HEADER + i * ITEM_HEADER;
                        let key = parse_key(&n[..], h);
                        if key < start {
                            continue;
                        }
                        let (off, len) = (le16(&n[..], h + 20) as usize, le16(&n[..], h + 22) as usize);
                        if off + len > BLOCK {
                            return Err(DAMAGED_LEAF);
                        }
                        if !f(key, &n[off..off + len]) {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                if HEADER + count * INTERIOR_ENTRY > BLOCK {
                    return Err("kfs: damaged tree node");
                }
                // From the last child whose first key is <= start.
                let mut first = 0;
                for i in 0..count {
                    if parse_key(&n[..], HEADER + i * INTERIOR_ENTRY) <= start {
                        first = i;
                    } else {
                        break;
                    }
                }
                (first..count).map(|i| Child::Disk(Bp::parse(&n[..], HEADER + i * INTERIOR_ENTRY + KEY))).collect()
            }
        };
        for k in kids {
            if !self.scan_child(k, start, f, depth + 1)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn get(&mut self, key: Key) -> Result<Option<Vec<u8>>, &'static str> {
        let mut found = None;
        self.scan(key, &mut |k, d| {
            if k == key {
                found = Some(Vec::from(d));
            }
            false
        })?;
        Ok(found)
    }

    /// The node `c` as one this operation may change: a node on disk is
    /// copied into memory, and its block freed (held until the commit).
    fn own(&mut self, c: Child) -> Result<usize, &'static str> {
        match c {
            Child::Mem(i) => Ok(i),
            Child::Disk(bp) => {
                let raw = self.node(bp)?;
                let n = Node::parse(&raw)?;
                self.free(bp);
                self.nodes.push(n);
                Ok(self.nodes.len() - 1)
            }
        }
    }

    fn own_root(&mut self) -> Result<usize, &'static str> {
        self.dirty = true;
        let r = self.own(self.root)?;
        self.root = Child::Mem(r);
        Ok(r)
    }

    /// Bytes the node needs, without copying it.
    fn size_of(&mut self, c: Child) -> Result<usize, &'static str> {
        match c {
            Child::Mem(i) => Ok(self.nodes[i].size()),
            Child::Disk(bp) => {
                let n = self.node(bp)?;
                let count = le16(&n[..], 6) as usize;
                if n[4] != 0 {
                    return Ok(HEADER + count * INTERIOR_ENTRY);
                }
                if HEADER + count * ITEM_HEADER > BLOCK {
                    return Err(DAMAGED_LEAF);
                }
                Ok(HEADER + (0..count).map(|i| ITEM_HEADER + le16(&n[..], HEADER + i * ITEM_HEADER + 22) as usize).sum::<usize>())
            }
        }
    }

    /// Adds an item, or replaces the one with the same key.
    fn insert(&mut self, key: Key, data: Vec<u8>) -> Result<(), &'static str> {
        let r = self.own_root()?;
        let extra = self.insert_at(r, key, data, 0)?;
        if !extra.is_empty() {
            // The root split: a new root above it and its new siblings.
            let first = self.nodes[r].first_key().unwrap_or(key);
            let level = self.nodes[r].level + 1;
            let mut kids = vec![(first, Child::Mem(r))];
            kids.extend(extra.into_iter().map(|(k, i)| (k, Child::Mem(i))));
            self.nodes.push(Node { level, items: Vec::new(), kids });
            self.root = Child::Mem(self.nodes.len() - 1);
        }
        Ok(())
    }

    /// Inserts under node `i`; returns the new siblings if `i` split.
    fn insert_at(&mut self, i: usize, key: Key, data: Vec<u8>, depth: u32) -> Result<Vec<(Key, usize)>, &'static str> {
        if depth > 16 {
            return Err(TOO_DEEP);
        }
        if self.nodes[i].level == 0 {
            let items = &mut self.nodes[i].items;
            match items.binary_search_by(|it| it.0.cmp(&key)) {
                Ok(p) => items[p].1 = data,
                Err(p) => items.insert(p, (key, data)),
            }
            return Ok(self.split(i));
        }
        let p = child_index(&self.nodes[i].kids, key);
        let c = self.own(self.nodes[i].kids[p].1)?;
        let entry = &mut self.nodes[i].kids[p];
        entry.1 = Child::Mem(c);
        if key < entry.0 {
            entry.0 = key;
        }
        let extra = self.insert_at(c, key, data, depth + 1)?;
        for (n, (k, s)) in extra.into_iter().enumerate() {
            self.nodes[i].kids.insert(p + 1 + n, (k, Child::Mem(s)));
        }
        Ok(self.split(i))
    }

    /// If node `i` no longer fits in a block, splits it; returns the new
    /// right-hand siblings with their first keys.
    fn split(&mut self, i: usize) -> Vec<(Key, usize)> {
        if self.nodes[i].size() <= BLOCK {
            return Vec::new();
        }
        let level = self.nodes[i].level;
        let mut parts: Vec<Node> = Vec::new();
        if level == 0 {
            // Even parts, as far as the item sizes allow.
            let items = core::mem::take(&mut self.nodes[i].items);
            let room = BLOCK - HEADER;
            let payload: usize = items.iter().map(|(_, d)| ITEM_HEADER + d.len()).sum();
            let target = payload / payload.div_ceil(room).max(2);
            let mut cur = Vec::new();
            let mut used = 0;
            for it in items {
                let need = ITEM_HEADER + it.1.len();
                if !cur.is_empty() && (used + need > room || used >= target) {
                    parts.push(Node::leaf(core::mem::take(&mut cur)));
                    used = 0;
                }
                used += need;
                cur.push(it);
            }
            parts.push(Node::leaf(cur));
        } else {
            let mut kids = core::mem::take(&mut self.nodes[i].kids);
            let right = kids.split_off(kids.len().div_ceil(2));
            parts.push(Node { level, items: Vec::new(), kids });
            parts.push(Node { level, items: Vec::new(), kids: right });
        }
        let mut parts = parts.into_iter();
        self.nodes[i] = parts.next().unwrap_or_default();
        parts
            .map(|n| {
                let k = n.first_key().unwrap_or_default();
                self.nodes.push(n);
                (k, self.nodes.len() - 1)
            })
            .collect()
    }

    /// Removes the item with this key, if there is one.
    fn remove(&mut self, key: Key) -> Result<Option<Vec<u8>>, &'static str> {
        let r = self.own_root()?;
        let old = self.remove_at(r, key, 0)?;
        // A root with one child gives way to it.
        while let Child::Mem(r) = self.root {
            let n = &mut self.nodes[r];
            if n.level == 0 || n.kids.len() > 1 {
                break;
            }
            match n.kids.first() {
                Some(k) => self.root = k.1,
                None => *n = Node::leaf(Vec::new()),
            }
        }
        Ok(old)
    }

    fn remove_at(&mut self, i: usize, key: Key, depth: u32) -> Result<Option<Vec<u8>>, &'static str> {
        if depth > 16 {
            return Err(TOO_DEEP);
        }
        if self.nodes[i].level == 0 {
            let items = &mut self.nodes[i].items;
            return Ok(items.binary_search_by(|it| it.0.cmp(&key)).ok().map(|p| items.remove(p).1));
        }
        let p = child_index(&self.nodes[i].kids, key);
        let c = self.own(self.nodes[i].kids[p].1)?;
        self.nodes[i].kids[p].1 = Child::Mem(c);
        let old = self.remove_at(c, key, depth + 1)?;
        self.rebalance(i, p)?;
        Ok(old)
    }

    /// After a removal under child `p` of node `i`: drops the child if
    /// it's empty, or merges it with a neighbour when both fit in one
    /// block, so deleting lots of files doesn't leave a sparse tree.
    fn rebalance(&mut self, i: usize, p: usize) -> Result<(), &'static str> {
        let Child::Mem(c) = self.nodes[i].kids[p].1 else { return Ok(()) };
        if self.nodes[c].is_empty() {
            self.nodes[i].kids.remove(p);
            return Ok(());
        }
        let used = self.nodes[c].size();
        if used > BLOCK / 4 {
            return Ok(());
        }
        for q in [p + 1, p.wrapping_sub(1)] {
            if q >= self.nodes[i].kids.len() {
                continue;
            }
            if used + self.size_of(self.nodes[i].kids[q].1)? - HEADER > BLOCK {
                continue;
            }
            let (l, r) = if q > p { (p, q) } else { (q, p) };
            let li = self.own(self.nodes[i].kids[l].1)?;
            let ri = self.own(self.nodes[i].kids[r].1)?;
            let right = core::mem::take(&mut self.nodes[ri]);
            self.nodes[li].items.extend(right.items);
            self.nodes[li].kids.extend(right.kids);
            self.nodes[i].kids[l].1 = Child::Mem(li);
            self.nodes[i].kids.remove(r);
            return Ok(());
        }
        Ok(())
    }

    // --- Commits ---

    /// Makes the operation's changes permanent (see the module docs).
    fn commit(&mut self) -> Result<(), &'static str> {
        if !self.dirty {
            return Ok(());
        }
        let txg = self.txg + 1;
        let root = self.write_child(self.root, txg)?;
        let bitmap_root = self.write_bitmap(txg)?;
        block::flush(self.disk)?;

        let mut sb = self.sb.clone();
        put64(&mut sb[..], 24, txg);
        root.put(&mut sb[..], 40);
        bitmap_root.put(&mut sb[..], 72);
        put64(&mut sb[..], 136, self.free_blocks());
        put64(&mut sb[..], 144, self.next_object);
        let sum = xxh64(&sb[..BLOCK - 8]);
        put64(&mut sb[..], BLOCK - 8, sum);
        write_blocks(self.disk, SB_FIRST + txg % SB_SLOTS, &sb[..])?;
        block::flush(self.disk)?;

        // Landed: what this commit freed can be reused now.
        self.sb = sb;
        self.txg = txg;
        self.root = Child::Disk(root);
        self.nodes.clear();
        self.held.fill(0);
        self.dirty = false;
        Ok(())
    }

    /// Back to the last commit, dropping the operation in progress (its
    /// data blocks were never part of a committed state, so they're
    /// simply free again).
    fn abort(&mut self) -> Result<(), &'static str> {
        self.nodes.clear();
        self.root = Child::Disk(Bp::parse(&self.sb[..], 40));
        self.next_object = le64(&self.sb[..], 144);
        self.dirty = false;
        self.load_bitmap()
    }

    /// Commits part of the way through a long operation, so its copied
    /// nodes don't pile up in memory. Only called between steps that each
    /// leave the tree consistent.
    fn maybe_commit(&mut self) -> Result<(), &'static str> {
        if self.nodes.len() > DIRTY_LIMIT { self.commit() } else { Ok(()) }
    }

    /// Writes node `c` and everything changed under it, children first.
    fn write_child(&mut self, c: Child, txg: u64) -> Result<Bp, &'static str> {
        let i = match c {
            Child::Disk(bp) => return Ok(bp),
            Child::Mem(i) => i,
        };
        for j in 0..self.nodes[i].kids.len() {
            let bp = self.write_child(self.nodes[i].kids[j].1, txg)?;
            self.nodes[i].kids[j].1 = Child::Disk(bp);
        }
        let raw = self.nodes[i].serialize(txg);
        let block = self.alloc(1)?;
        write_blocks(self.disk, block, &raw[..])?;
        let checksum = xxh64(&raw[..]);
        self.cache.put(block, checksum, &raw);
        Ok(Bp { block, count: 1, kind: BP_NODE, birth: txg, checksum })
    }

    /// Writes the changed bitmap blocks and a new index for them; returns
    /// the index's root.
    fn write_bitmap(&mut self, txg: u64) -> Result<Bp, &'static str> {
        let n = self.bitmap.len();
        // The index points at every bitmap block, so it's rewritten whole.
        for bp in core::mem::take(&mut self.index) {
            self.free(bp);
        }
        let mut nodes = 0;
        let mut m = n;
        loop {
            m = m.div_ceil(INDEX_MAX);
            nodes += m;
            if m == 1 {
                break;
            }
        }
        let mut index_blocks = Vec::with_capacity(nodes);
        for _ in 0..nodes {
            index_blocks.push(self.alloc(1)?);
        }
        // Every changed bitmap block moves. Moving one changes bits,
        // perhaps in a block that has already moved or one that hasn't
        // changed yet, so go round until every changed block has moved.
        let mut moved = vec![false; n];
        while let Some(k) = (0..n).find(|&k| self.bitmap_dirty[k] && !moved[k]) {
            moved[k] = true;
            let old = self.bitmap[k];
            let block = self.alloc(1)?;
            self.free(old);
            self.bitmap[k].block = block;
        }
        // The bits are final now.
        let mut buf: Box<[u8; BLOCK]> = Box::new([0u8; BLOCK]);
        for k in (0..n).filter(|&k| moved[k]) {
            for w in 0..WORDS_PER_BITMAP {
                put64(&mut buf[..], w * 8, self.bits[k * WORDS_PER_BITMAP + w]);
            }
            write_blocks(self.disk, self.bitmap[k].block, &buf[..])?;
            self.bitmap[k] = Bp { block: self.bitmap[k].block, count: 1, kind: BP_BITMAP, birth: txg, checksum: xxh64(&buf[..]) };
        }
        self.bitmap_dirty.fill(false);
        // The index, bottom up.
        let mut level = self.bitmap.clone();
        let mut depth = 0;
        let mut next = index_blocks.into_iter();
        loop {
            depth += 1;
            let mut up = Vec::new();
            for group in level.chunks(INDEX_MAX) {
                buf.fill(0);
                buf[0..4].copy_from_slice(MAGIC_NODE);
                buf[4] = depth;
                buf[5] = TREE_BITMAP;
                put16(&mut buf[..], 6, group.len() as u16);
                put64(&mut buf[..], 8, txg);
                for (j, bp) in group.iter().enumerate() {
                    bp.put(&mut buf[..], HEADER + j * BP);
                }
                let block = next.next().ok_or("kfs: bitmap index miscounted")?;
                write_blocks(self.disk, block, &buf[..])?;
                let bp = Bp { block, count: 1, kind: BP_BITMAP_INDEX, birth: txg, checksum: xxh64(&buf[..]) };
                self.index.push(bp);
                up.push(bp);
            }
            level = up;
            if level.len() == 1 {
                return Ok(level[0]);
            }
        }
    }

    // --- Files and folders ---

    fn inode(&mut self, obj: u64) -> Result<Inode, &'static str> {
        Inode::parse(&self.get((obj, K_INODE, 0))?.ok_or("kfs: missing inode (damaged)")?)
    }

    fn put_inode(&mut self, obj: u64, ino: &Inode) -> Result<(), &'static str> {
        self.insert((obj, K_INODE, 0), ino.bytes())
    }

    /// The child called `name` in directory `dir`: `(object, is_dir,
    /// entry's key offset)`.
    fn lookup(&mut self, dir: u64, name: &str) -> Result<Option<(u64, bool, u64)>, &'static str> {
        let base = name_hash(name.as_bytes());
        let mut found = None;
        self.scan((dir, K_DIR_ENTRY, base), &mut |k, d| {
            if k.0 != dir || k.1 != K_DIR_ENTRY || k.2.wrapping_sub(base) >= 8 {
                return false;
            }
            if d.len() >= 10 && d.get(10..10 + d[9] as usize) == Some(name.as_bytes()) {
                found = Some((le64(d, 0), d[8] == T_DIR, k.2));
                return false;
            }
            true
        })?;
        Ok(found)
    }

    /// The object at `path` (absolute within the volume): `(object, is_dir)`.
    fn resolve(&mut self, path: &str) -> Result<(u64, bool), &'static str> {
        let mut cur = (ROOT_OBJECT, true);
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !cur.1 {
                return Err("not a directory");
            }
            let (obj, is_dir, _) = self.lookup(cur.0, part)?.ok_or("no such file or directory")?;
            cur = (obj, is_dir);
        }
        Ok(cur)
    }

    /// The folder holding `path`, which must exist and be a folder, and
    /// the last part of `path`.
    fn parent<'a>(&mut self, path: &'a str) -> Result<(u64, &'a str), &'static str> {
        let (parent, name) = split_path(path)?;
        match self.resolve(parent)? {
            (dir, true) => Ok((dir, name)),
            _ => Err("not a directory"),
        }
    }

    /// `(name, object, is_dir, key offset)` for everything in `dir`.
    fn entries(&mut self, dir: u64) -> Result<Vec<(String, u64, bool, u64)>, &'static str> {
        let mut out = Vec::new();
        self.scan((dir, K_DIR_ENTRY, 0), &mut |k, d| {
            if k.0 != dir || k.1 != K_DIR_ENTRY {
                return false;
            }
            if d.len() >= 10 {
                let name = d.get(10..10 + d[9] as usize).unwrap_or(&[]);
                out.push((String::from(core::str::from_utf8(name).unwrap_or("?")), le64(d, 0), d[8] == T_DIR, k.2));
            }
            true
        })?;
        Ok(out)
    }

    fn add_entry(&mut self, dir: u64, name: &str, child: u64, is_dir: bool) -> Result<(), &'static str> {
        // The first of the eight slots for this hash that's free.
        let base = name_hash(name.as_bytes());
        let mut used = [false; 8];
        self.scan((dir, K_DIR_ENTRY, base), &mut |k, _| {
            if k.0 != dir || k.1 != K_DIR_ENTRY || k.2.wrapping_sub(base) >= 8 {
                return false;
            }
            used[(k.2 - base) as usize] = true;
            true
        })?;
        let slot = used.iter().position(|u| !u).ok_or("too many names in this folder look alike to KFS; pick another name")?;
        let mut d = Vec::with_capacity(10 + name.len());
        d.extend_from_slice(&child.to_le_bytes());
        d.push(if is_dir { T_DIR } else { T_FILE });
        d.push(name.len() as u8);
        d.extend_from_slice(name.as_bytes());
        self.insert((dir, K_DIR_ENTRY, base + slot as u64), d)
    }

    /// A folder's contents changed: new modification time, and `links`
    /// changed by the number of folders added (one per subfolder).
    fn touch(&mut self, dir: u64, links: i32) -> Result<(), &'static str> {
        let mut ino = self.inode(dir)?;
        ino.links = ino.links.saturating_add_signed(links);
        let now = now_ns();
        ino.modified = now;
        ino.changed = now;
        self.put_inode(dir, &ino)
    }

    fn new_object(&mut self) -> u64 {
        let obj = self.next_object;
        self.next_object += 1;
        obj
    }

    /// Removes an object's items: only its data, or all of them. Extents'
    /// blocks are freed.
    fn drop_items(&mut self, obj: u64, data_only: bool) -> Result<(), &'static str> {
        let mut doomed: Vec<(Key, Option<Bp>)> = Vec::new();
        self.scan((obj, 0, 0), &mut |k, d| {
            if k.0 != obj {
                return false;
            }
            if !data_only || k.1 == K_INLINE || k.1 == K_EXTENT {
                doomed.push((k, (k.1 == K_EXTENT && d.len() >= BP).then(|| Bp::parse(d, 0))));
            }
            true
        })?;
        for (k, bp) in doomed {
            if let Some(bp) = bp {
                self.free(bp);
            }
            self.remove(k)?;
        }
        Ok(())
    }

    /// Stores `data` as object `obj`'s contents (it has none yet): inline
    /// if it's small, otherwise in extents written to newly allocated
    /// blocks, in as few runs as free space allows. Returns the blocks used.
    fn write_data(&mut self, obj: u64, data: &[u8]) -> Result<u64, &'static str> {
        if data.len() <= INLINE_MAX {
            self.insert((obj, K_INLINE, 0), Vec::from(data))?;
            return Ok(0);
        }
        let txg = self.txg + 1;
        let nblocks = data.len().div_ceil(BLOCK) as u64;
        let mut pad: Vec<u8> = Vec::new();
        let mut done = 0;
        let reserve = RESERVE_MIN.max(self.total_blocks / 256);
        while done < nblocks {
            let room = self.available().saturating_sub(reserve);
            if room == 0 {
                return Err("the KFS disk is full");
            }
            let (start, got) = self.find_run((nblocks - done).min(room)).ok_or("the KFS disk is full")?;
            self.set_used(start, got);
            let mut k = 0;
            while k < got {
                let count = (got - k).min(EXTENT_BLOCKS);
                let at = ((done + k) as usize) * BLOCK;
                let bytes = &data[at..(at + count as usize * BLOCK).min(data.len())];
                // The last extent's final block is padded with zeros.
                let whole: &[u8] = if bytes.len() == count as usize * BLOCK {
                    bytes
                } else {
                    pad.clear();
                    pad.extend_from_slice(bytes);
                    pad.resize(count as usize * BLOCK, 0);
                    &pad
                };
                write_blocks(self.disk, start + k, whole)?;
                let bp = Bp { block: start + k, count: count as u32, kind: BP_DATA, birth: txg, checksum: xxh64(whole) };
                let mut item = vec![0u8; BP + 8];
                bp.put(&mut item, 0);
                put64(&mut item, BP, bytes.len() as u64);
                self.insert((obj, K_EXTENT, at as u64), item)?;
                k += count;
            }
            done += got;
        }
        Ok(nblocks)
    }

    fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), &'static str> {
        let (dir, name) = self.parent(path)?;
        if !valid_name(name) {
            return Err("invalid file name");
        }
        let now = now_ns();
        match self.lookup(dir, name)? {
            Some((_, true, _)) => Err("is a directory"),
            Some((obj, false, _)) => {
                // The old contents' blocks stay untouched (held) until
                // this commit lands; the new ones go elsewhere.
                self.drop_items(obj, true)?;
                let blocks = self.write_data(obj, data)?;
                let mut ino = self.inode(obj)?;
                ino.size = data.len() as u64;
                ino.blocks = blocks;
                ino.modified = now;
                ino.changed = now;
                self.put_inode(obj, &ino)
            }
            None => {
                let obj = self.new_object();
                let blocks = self.write_data(obj, data)?;
                self.put_inode(obj, &Inode::new(S_IFREG | 0o644, data.len() as u64, 1, now, blocks))?;
                self.add_entry(dir, name, obj, false)?;
                self.touch(dir, 0)
            }
        }
    }

    fn create_dir(&mut self, path: &str) -> Result<(), &'static str> {
        let (dir, name) = self.parent(path)?;
        if !valid_name(name) {
            return Err("invalid folder name");
        }
        if self.lookup(dir, name)?.is_some() {
            return Err("already exists");
        }
        let obj = self.new_object();
        self.put_inode(obj, &Inode::new(S_IFDIR | 0o755, 0, 2, now_ns(), 0))?;
        self.add_entry(dir, name, obj, true)?;
        self.touch(dir, 1)
    }

    /// Deletes a file or folder, everything in a folder included.
    fn remove_path(&mut self, path: &str, files_only: bool) -> Result<(), &'static str> {
        if is_root(path) {
            return Err("can't delete the root folder");
        }
        let (dir, name) = self.parent(path)?;
        let (obj, is_dir, off) = self.lookup(dir, name)?.ok_or("no such file or directory")?;
        if is_dir && files_only {
            return Err("is a directory");
        }
        if is_dir {
            self.empty_dir(obj, 0)?;
        }
        self.drop_items(obj, false)?;
        self.remove((dir, K_DIR_ENTRY, off))?;
        self.touch(dir, if is_dir { -1 } else { 0 })
    }

    /// Deletes everything in folder `dir`, one entry at a time, so a
    /// commit part of the way through (see `maybe_commit`) leaves a
    /// consistent tree with some of it deleted.
    fn empty_dir(&mut self, dir: u64, depth: u32) -> Result<(), &'static str> {
        if depth > 64 {
            return Err("folders nested too deeply");
        }
        for (_, child, is_dir, off) in self.entries(dir)? {
            if is_dir {
                self.empty_dir(child, depth + 1)?;
            }
            self.drop_items(child, false)?;
            self.remove((dir, K_DIR_ENTRY, off))?;
            if is_dir {
                self.touch(dir, -1)?;
            }
            self.maybe_commit()?;
        }
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), &'static str> {
        if is_root(from) || is_root(to) {
            return Err("can't move the root folder");
        }
        let (fdir, fname) = self.parent(from)?;
        let (obj, is_dir, off) = self.lookup(fdir, fname)?.ok_or("no such file or directory")?;
        let (tdir, tname) = self.parent(to)?;
        if !valid_name(tname) {
            return Err("invalid name");
        }
        if let Some((other, _, _)) = self.lookup(tdir, tname)? {
            return if other == obj { Ok(()) } else { Err("something with that name is already there") };
        }
        if is_dir && to.strip_prefix(from).is_some_and(|rest| rest.starts_with('/')) {
            return Err("can't move a folder into itself");
        }
        self.remove((fdir, K_DIR_ENTRY, off))?;
        self.add_entry(tdir, tname, obj, is_dir)?;
        let moved_dirs = is_dir as i32;
        if fdir == tdir {
            self.touch(fdir, 0)?;
        } else {
            self.touch(fdir, -moved_dirs)?;
            self.touch(tdir, moved_dirs)?;
        }
        let mut ino = self.inode(obj)?;
        ino.changed = now_ns();
        self.put_inode(obj, &ino)
    }

    // --- Reading files ---

    /// The pieces of object `obj` that overlap byte range `from..to`.
    fn pieces(&mut self, obj: u64, from: u64, to: u64) -> Result<Vec<Piece>, &'static str> {
        let mut out = Vec::new();
        // Inline data comes before extents in key order; one search finds
        // either kind. An extent covering `from` starts at most EXTENT_MAX
        // bytes before it.
        if let Some(d) = self.get((obj, K_INLINE, 0))? {
            out.push(Piece::Inline(d));
            return Ok(out);
        }
        let start = from.saturating_sub(EXTENT_MAX - 1);
        self.scan((obj, K_EXTENT, start), &mut |k, d| {
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

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, &'static str> {
        let (obj, is_dir) = self.resolve(path)?;
        if is_dir {
            return Err("is a directory");
        }
        let size = self.inode(obj)?.size;
        // Room for whole blocks, so extents can be read straight in.
        let mut data = Vec::new();
        let rounded = (size as usize).div_ceil(BLOCK) * BLOCK;
        data.try_reserve_exact(rounded).map_err(|_| "file buffer allocation failed")?;
        data.resize(rounded.max(size as usize), 0);
        for p in self.pieces(obj, 0, size)? {
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
                    read_extent(self.disk, bp, &mut data[at..at + bytes])?;
                }
            }
        }
        data.truncate(size as usize);
        Ok(data)
    }

    fn read_at(&mut self, file: &File, offset: u64, out: &mut [u8]) -> Result<usize, &'static str> {
        if offset >= file.size || out.is_empty() {
            return Ok(0);
        }
        let want = out.len().min((file.size - offset) as usize);
        let end = offset + want as u64;
        out[..want].fill(0);
        let mut buf: Vec<u8> = Vec::new();
        for p in self.pieces(file.obj, offset, end)? {
            let (start, bytes): (u64, &[u8]) = match &p {
                Piece::Inline(d) => (0, &d[..]),
                Piece::Extent { offset: at, length, bp } => {
                    buf.resize(bp.count as usize * BLOCK, 0);
                    read_extent(self.disk, *bp, &mut buf)?;
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

/// One piece of a file: inline data, or an extent of whole blocks.
enum Piece {
    Inline(Vec<u8>),
    Extent { offset: u64, length: u64, bp: Bp },
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

// --- The interface `vfs.rs` uses. Paths are absolute within the volume. ---

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

pub fn list_dir(path: &str) -> Result<Vec<Entry>, &'static str> {
    with(|fs| {
        let (dir, is_dir) = fs.resolve(path)?;
        if !is_dir {
            return Err("not a directory");
        }
        let mut out = Vec::new();
        for (name, obj, is_dir, _) in fs.entries(dir)? {
            let size = if is_dir { 0 } else { fs.inode(obj)?.size };
            out.push(Entry { name, is_dir, size });
        }
        Ok(out)
    })
}

/// `(is_dir, size)` of whatever `path` names.
pub fn stat(path: &str) -> Result<(bool, u64), &'static str> {
    with(|fs| {
        let (obj, _) = fs.resolve(path)?;
        let ino = fs.inode(obj)?;
        Ok((ino.is_dir(), ino.size))
    })
}

pub fn read_file(path: &str) -> Result<Vec<u8>, &'static str> {
    with(|fs| fs.read_file(path))
}

/// An open file: reads at any offset without reading the rest.
#[derive(Clone, Copy)]
pub struct File {
    obj: u64,
    pub size: u64,
}

pub fn open_file(path: &str) -> Result<File, &'static str> {
    with(|fs| {
        let (obj, is_dir) = fs.resolve(path)?;
        if is_dir {
            return Err("is a directory");
        }
        Ok(File { obj, size: fs.inode(obj)?.size })
    })
}

impl File {
    /// Fills `out` from byte `offset`; returns how much was read (less at
    /// the end of the file). Unwritten ranges read as zeros.
    pub fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, &'static str> {
        with(|fs| fs.read_at(self, offset, out))
    }
}

/// Creates or replaces a file.
pub fn write_file(path: &str, data: &[u8]) -> Result<(), &'static str> {
    change(|fs| fs.write_file(path, data))
}

pub fn create_dir(path: &str) -> Result<(), &'static str> {
    change(|fs| fs.create_dir(path))
}

pub fn remove_file(path: &str) -> Result<(), &'static str> {
    change(|fs| fs.remove_path(path, true))
}

/// Deletes a file, or a folder with everything in it.
pub fn remove(path: &str) -> Result<(), &'static str> {
    change(|fs| fs.remove_path(path, false))
}

/// Renames or moves a file or folder within the volume.
pub fn rename(from: &str, to: &str) -> Result<(), &'static str> {
    change(|fs| fs.rename(from, to))
}
