# KonjacFS (KFS) design

KFS is the filesystem that will replace FAT16 as KonjacOS's main disk
format. This document fixes its on-disk layout and the rules for changing
it, before any code is written. Status: **proposal, not implemented**.

## Goals

1. **Never corrupt on power loss.** Every change is copy-on-write: live
   data is never overwritten in place, and a single superblock write
   switches the disk from the old state to the new one.
2. **Detect corruption.** Every block is checksummed, and the checksum is
   stored in the pointer to it (a Merkle tree rooted in the superblock).
3. **Fast on this kernel.** Files are stored as extents (runs of
   contiguous blocks) and read in large requests through `block.rs`.
   Small files live inside their metadata record and cost one read.
4. **Bounded memory.** Metadata is cached in a fixed-size cache; nothing
   in RAM grows with the number of files.
5. **Room to grow.** The format reserves what snapshots, instant copies
   (reflinks), sparse files and compression need, so adding them later
   doesn't change the format.

### Not in version 1

Snapshots, reflinks, compression, background scrubbing, defragmentation,
direct I/O, extended attributes, permissions beyond a mode field, and
volumes larger than one disk. Each is listed under
[Milestones](#milestones), with what the v1 format already reserves for it.

## Conventions

- **Block size: 4096 bytes.** Every structure is in blocks; the block
  layer still talks in 512-byte sectors (8 per block).
- **Block numbers are 64-bit.** With 4 KiB blocks that's 64 ZiB.
- All integers are **little-endian**. Structures are packed, with
  explicit padding.
- **Checksum: xxHash64** (seed 0) for blocks and for the superblock. It's
  fast and catches bit rot; it isn't meant to resist deliberate tampering.
- **txg** (transaction group): a counter that goes up by one with each
  commit. Every block records the txg it was written in.

## Disk layout

```
block 0          reserved (zeros; room for a boot sector later)
blocks 1-8       superblock ring (8 copies, one written per commit)
blocks 9-15      reserved
block 16 onward  everything else: tree nodes, file data, free-space bitmap
```

Nothing after block 15 has a fixed position. The superblock says where
everything is.

### Superblock (one 4 KiB block)

| Offset | Size | Field | Meaning |
|---:|---:|---|---|
| 0 | 8 | `magic` | `"KONJACFS"` |
| 8 | 4 | `version` | 1 |
| 12 | 4 | `block_size` | 4096 |
| 16 | 8 | `total_blocks` | size of the volume |
| 24 | 8 | `txg` | the commit this superblock belongs to |
| 32 | 8 | `features` | bits for optional features (all 0 in v1) |
| 40 | 32 | `fs_root` | block pointer to the filesystem tree's root |
| 72 | 32 | `bitmap_root` | block pointer to the free-space bitmap's index |
| 104 | 32 | `snap_root` | block pointer to the snapshot tree (zero in v1) |
| 136 | 8 | `free_blocks` | free block count, for `statfs` |
| 144 | 8 | `next_object` | next unused object (inode) number |
| 152 | 16 | `uuid` | identifies the volume |
| 168 | 32 | `label` | volume name, UTF-8, zero-padded |
| 200 | ... | reserved | zeros |
| 4088 | 8 | `checksum` | xxHash64 of bytes 0..4088 |

**Mounting:** read all eight slots and use the valid one (right magic,
correct checksum) with the highest `txg`. A commit writes slot
`txg % 8`, so a torn or failed superblock write only loses that one slot.
The previous seven are still there, each pointing at an older but
complete state.

### Block pointer (32 bytes)

Every reference from one block to another is a block pointer:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `block` -- first block number (0 = none) |
| 8 | 4 | `count` -- how many contiguous blocks |
| 12 | 1 | `kind` -- tree node, data, bitmap... |
| 13 | 1 | `compression` -- 0 in v1 (reserved for LZ4) |
| 14 | 2 | reserved |
| 16 | 8 | `birth_txg` -- the commit that wrote these blocks |
| 24 | 8 | `checksum` -- xxHash64 of all `count` blocks |

Reading through a pointer always verifies the checksum, so corruption
anywhere is caught on read and reported as an I/O error rather than
returned as data. `birth_txg` is what snapshots will use to decide
whether a block can be freed (see [Snapshots](#snapshots-v2)).

## The filesystem tree

All metadata lives in **one copy-on-write B+tree**, like btrfs's
filesystem tree. Every record is an *item* with a fixed-size key:

| Field | Size | |
|---|---:|---|
| `object` | 8 | the inode number the item belongs to |
| `kind` | 1 | what the item is (table below) |
| `offset` | 8 | meaning depends on `kind` |

Items are sorted by `(object, kind, offset)`. Everything about one file
therefore sits together in the tree, and a file's extents are in order of
file offset.

| `kind` | `offset` means | Item contents |
|---|---|---|
| `INODE` (1) | 0 | the inode (below) |
| `DIR_ENTRY` (2) | hash of the name | child inode number, type, name |
| `INLINE` (3) | 0 | the whole file's data |
| `EXTENT` (4) | byte offset in the file | a block pointer + length in bytes |

### Nodes

A node is one 4 KiB block:

- **Header (64 bytes):** magic `"KFSN"`, `level` (0 = leaf), item count,
  the owning tree, `birth_txg`, and padding. The node's checksum isn't in
  the node itself; it's in the parent's pointer to it.
- **Interior node:** keys and child block pointers, about 80 per node.
- **Leaf:** an array of `(key, data offset, data size)` from the front, and
  item data packed from the back of the block, like a slotted page.

The tree is a standard B+tree, with one rule: **a node is never modified
in place.** Changing a leaf writes a new copy of it, which changes its
parent's pointer, so the parent gets a new copy too, and so on up to the
root. The new root goes into the next superblock. This is what makes a
commit atomic, and it's also why changes are batched (see
[Commits](#commits)): ten changes to one leaf in a commit write it once,
not ten times.

### Inode (128 bytes)

| Field | Size | |
|---|---:|---|
| `mode` | 4 | file type (file, directory, symlink) + permission bits |
| `flags` | 4 | `INLINE`, `SPARSE`, reserved bits |
| `size` | 8 | length in bytes |
| `links` | 4 | hard link count |
| `generation` | 4 | bumped when the inode number is reused |
| `created`, `modified`, `changed`, `accessed` | 4 x 8 | nanoseconds since 1970 |
| `blocks` | 8 | blocks allocated (less than size/4096 for a sparse file) |
| reserved | rest | zeros |

Object 1 is the root directory. Object numbers are never reused while the
generation counter could be confused (v1 simply never reuses them).

### Directories

Each entry is a `DIR_ENTRY` item under the directory's object, keyed by a
64-bit hash of the name. The item holds the name itself (up to 255 bytes
of UTF-8), so a hash collision is resolved by comparing names; colliding
entries take the next free `offset`. Looking a name up costs one tree
search.

**Names are case-sensitive and case-preserving**, like Linux. FAT16 is
case-insensitive; the Java and glibc programs that run here expect Linux
behaviour. The desktop can still offer case-insensitive matching on top
(Start's search already does).

`readdir` walks a directory's entries in hash order. To list a directory
in name order, the caller sorts the entries, as Files already does.

### File data

- **Inline:** a file of up to 2 KiB has its data in an `INLINE` item next
  to its inode. Reading it costs the one leaf read that found the inode.
  This covers configuration files, `DESKTOP.CFG` and most notes.
- **Extents:** larger files are a list of `EXTENT` items, each mapping a
  byte range of the file to a run of up to 32768 blocks (128 MiB). Writing
  a file in one go allocates one large run when free space allows, so
  reading it back is one request per 64 KiB (the virtio-blk limit) with
  no lookups in between.
- **Sparse:** a range with no extent reads as zeros and uses no space.
  The format supports it from v1; the write path creates holes only
  where something explicitly seeks past the end.

When a file grows or shrinks across 2 KiB it moves between inline and
extents.

## Free space

v1 keeps free space as a **bitmap**: one bit per block, 32 KiB of bitmap
per GiB of disk (1 MiB for a 32 GiB disk). The bitmap is stored in
4 KiB blocks found through an index (`bitmap_root`) and is copy-on-write
like everything else.

- At mount, the bitmap blocks are read as needed rather than all at once.
  The allocator keeps a small in-memory summary (free blocks per 16 MiB
  region) to find space without scanning everything.
- **Allocating** looks for a free run big enough for the whole write,
  close to the file's previous extent, so files stay contiguous.
- **Freeing is deferred:** a block freed in txg N is still part of the
  last committed state until txg N commits. It goes on a pending list and
  becomes allocatable only after the commit. Reusing it sooner would
  corrupt the old state that a crash would fall back to.
- The bitmap blocks a commit changes are themselves allocated during that
  commit, from space that was already free at its start. The allocator
  never needs to allocate in order to record an allocation, which avoids
  the chicken-and-egg problem a free-space *tree* has.

A range tree that uses less space on huge, fragmented disks can replace
the bitmap later (feature bit), but at hobby-OS disk sizes the bitmap is
small and simple.

## Commits

All changes go into a transaction group in memory:

1. **Changes accumulate.** Writes, renames and deletes change cached tree
   nodes and data buffers, marking them dirty. Nothing on disk changes.
2. **A commit starts** every 5 seconds, when the dirty data passes a limit
   (4 MiB by default), on `fsync`, or at shutdown.
3. **Data blocks are written** to newly allocated locations.
4. **Dirty tree nodes are written bottom-up**, each to a new location,
   each parent getting its child's new pointer and checksum. Then the
   bitmap blocks.
5. **Flush:** the disk is asked to make everything durable (virtio-blk's
   flush request).
6. **The superblock** for `txg + 1` is written to slot `(txg + 1) % 8`.
7. **Flush again.** The commit is now permanent. Blocks freed during this
   txg become allocatable.

A crash anywhere before step 6 finishes leaves the previous superblock as
the newest valid one, which points only at blocks that were never
overwritten. Nothing needs repairing at mount: there's no journal to
replay and no `fsck` to run.

**What a crash can lose** is the last few seconds of changes since the
previous commit, as on every modern filesystem. `fsync` (and Notepad's
Save) forces a commit for the data that has to survive.

## Memory

- **Node cache:** a fixed-size LRU of tree nodes, 1 MiB by default. Nodes
  near the root are touched on every lookup, so they stay in the cache
  naturally; a lookup in a tree with millions of files costs at most a
  few disk reads.
- **Dirty nodes** are pinned in memory until their commit. If they fill
  half the cache, a commit is started early, so memory use stays bounded.
- **Data** isn't cached by KFS in v1 (there's no page cache in the kernel
  yet). Reads go straight from the disk into the caller's buffer, through
  `block.rs`.

## How it fits into the kernel

Today every part of the kernel calls `fat16::` directly. KFS adds a small
**VFS layer** (`vfs.rs`) with the operations already used (`read_file`,
`open_file`/`read_at`, `write_file`, `list_dir`, `stat_path`,
`create_dir`, `rename`, `remove`, `copy`), which dispatches by mount point:

- `/` is KFS once a KFS disk is present.
- The FAT16 disk stays available at `/fat` for moving files to and from
  the host. With no KFS disk, FAT16 is `/` as it is today.

The desktop, the shell, the Linux syscall layer, the loader and DOOM
switch from `fat16::` to `vfs::`, which is a mechanical change.

## Tools

- **`tools/mkkfs.py`** builds a KFS image from a folder, the way `mtools`
  builds the FAT16 image from `disk_root/` today, so `make` keeps working
  with no new system packages. It's written from this document, separately
  from the kernel code: if the two disagree about the format, the tests
  catch it.
- **`tools/kfsck.py`** checks an image: every checksum, every tree's
  ordering, the bitmap against what the trees reference, and link counts.
  The tests run it after every scenario.
- **`tools/kfs.py ls|cat|get`** reads files out of an image on the host.

## Testing

- **Format round-trip:** `mkkfs.py` builds an image, the kernel reads
  every file back and compares checksums.
- **Behaviour:** the Files/Notepad/DOOM scenarios from the FAT16 work,
  then `kfsck.py`.
- **Crash testing**, which is what backs the "never corrupts" goal: a
  shell command writes, renames and deletes in a loop while the harness
  kills QEMU at a random moment. The image must then pass `kfsck.py` and
  mount, with every file either at its last committed contents or absent.
  This runs hundreds of times with different timings.
- **Bit rot:** flip a byte in an image; reading the affected file must
  return an error, not wrong data, and everything else must stay
  readable.

## Milestones

Each one ends in something that boots and passes its tests.

1. **Read-only KFS.** `mkkfs.py`, the VFS layer, and a kernel that mounts
   and reads a KFS disk. FAT16 stays the default until 3.
2. **Writing.** Commits, the allocator, deferred frees, inline data and
   extents. Files and Notepad work on KFS.
3. **Crash-tested, then default.** Crash and bit-rot testing pass, `make`
   builds a KFS disk, FAT16 moves to `/fat`. Released as a new version.
4. **Snapshots** (format already ready: `snap_root`, `birth_txg`).
5. **Reflinks and sparse writes** (instant copies in Files).
6. **LZ4 compression** (the `compression` byte in block pointers).
7. **Scrubbing and defragmentation.**
8. **Direct I/O and a page cache**, once there are DMA drivers worth
   bypassing a cache for.

### Snapshots (v2)

A snapshot records a superblock's roots and txg in the snapshot tree. It
copies no data and takes one commit.

What makes them work is freeing: when a commit stops referencing a block,
the block is freed only if its `birth_txg` is newer than the latest
snapshot (no snapshot can see it). Otherwise it goes on that snapshot's
*deadlist*. Deleting a snapshot walks its deadlist and frees the blocks
no other snapshot still needs. This is ZFS's scheme. It needs no
per-block reference counts, which is why the v1 format already records
`birth_txg` in every pointer.

Rolling back to a snapshot makes its roots the current ones, which is the
"freeze the system before running risky code" idea.

### Reflinks (v2)

Copying a file in Files creates new `EXTENT` items pointing at the same
blocks, so the copy is instant. Blocks shared this way need reference
counts (a small refcount tree, added with this milestone); copy-on-write
already guarantees that changing one copy never changes the other.

## The original ideas, and where they went

| Idea | In this design |
|---|---|
| Extents instead of cluster chains | v1: `EXTENT` items, up to 128 MiB each |
| 64-bit block addressing | v1 |
| Copy-on-write + atomic superblock switch | v1: [Commits](#commits), 8-slot superblock ring |
| Small files inside the index record | v1: `INLINE`, up to 2 KiB |
| Checksums in parent pointers (Merkle tree) | v1: xxHash64 in every block pointer |
| Bounded LRU cache / "on-demand node paging" | v1: one 1 MiB node cache |
| Sequential write buffer | v1: transaction groups (5 s / 4 MiB) |
| Instant snapshots | Milestone 4; format ready in v1 |
| Block cloning (reflinks) | Milestone 5 |
| Sparse files | Format in v1, write path in milestone 5 |
| Inline compression | Milestone 6; LZ4 first (fast, simple), Zstd maybe later |
| Background scrubbing | Milestone 7 |
| CoW aging / defragmentation | Milestone 7; reduced up front by allocating whole runs |
| Direct I/O for the JVM | Milestone 8: needs a page cache to bypass first |
| "A few KB of RAM at any size" | Replaced by a fixed, configurable cache (1 MiB); a few KB would make every lookup a disk read |
| BLAKE3 | xxHash64 instead: detecting bit rot doesn't need a cryptographic hash, and BLAKE3 costs far more CPU under QEMU |

## Open questions

1. **Case sensitivity.** This document picks case-sensitive (Linux
   behaviour). The other option is FAT-like case-insensitive lookups.
2. **Default disk size** for `make`: FAT16's limit is 2 GiB; KFS could
   default to 512 MiB and grow later.
3. **Keep FAT16 at `/fat`** after the switch, or drop it once `mkkfs.py`
   and `kfs.py` cover moving files to and from the host.
