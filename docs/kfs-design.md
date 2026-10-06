# KonjacFS (KFS) design

KFS is the filesystem that will replace FAT16 as KonjacOS's main disk
format. This document fixes its on-disk layout and the rules for changing
it. Status: **milestones 1-3 implemented**: KFS reads and writes, has
passed the crash and bit-rot tests, and is KonjacOS's main disk (`/`).
The details below match `tools/kfs.py` and `kernel/src/kfs.rs`.

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

The older slots point at older states, but only the **previous one** is
guaranteed complete. Blocks a commit frees are reusable as soon as it
lands, so commit N+1 may overwrite what N-1 used; N-1 itself stays whole
until a commit after the newest one happens. (An earlier draft said all
seven older slots were complete; that would need frees held back for
eight commits, and across reboots.) So mounting tries the newest
superblock and, if its tree root or bitmap fails its checksum, the next
newest, saying so at boot. Going further back is a last resort: anything
reused since then fails its checksum rather than being read as data.
`kfs.py mkfs` writes the first superblock to all eight slots, so even a
fresh image survives one damaged slot.

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
- **Interior node:** up to 72 entries of 56 bytes: a key (24 bytes:
  `object` u64, `offset` u64, `kind` u8, 7 bytes padding) and the
  child's block pointer. The key is the smallest key in that child.
- **Leaf:** 24-byte item headers from the front (`object` u64, `offset`
  u64, `kind` u8, 3 bytes padding, data offset u16, data size u16), and
  item data packed from the back of the block, like a slotted page.

The tree is a standard B+tree, with one rule: **a node is never modified
in place.** Changing a leaf writes a new copy of it, which changes its
parent's pointer, so the parent gets a new copy too, and so on up to the
root. The new root goes into the next superblock. This is what makes a
commit atomic, and it's also why changes are batched (see
[Commits](#commits)): ten changes to one leaf in a commit write it once,
not ten times.

A node that overflows splits into as many nodes as its items need
(usually two). After a removal, an empty node is dropped, and a node
under a quarter full is merged into a neighbour if the two fit in one
block; a root left with one child gives way to it. Interior keys are
lower bounds: the key for a child is never more than the smallest key
in it.

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

Each entry is a `DIR_ENTRY` item under the directory's object, keyed by
the xxHash64 of the name with its low three bits cleared. The item holds
the child's object number (u64), its type (u8: 1 file, 2 folder), the
name's length (u8) and the name itself (up to 255 bytes of UTF-8). A hash
collision is resolved by comparing names: colliding entries take the
next free `offset` among the following seven. Looking a name up costs
one tree search.

**Names are case-sensitive and case-preserving**, like Linux (decided):
`Notes.txt` and `notes.txt` are different files. FAT16 is
case-insensitive; the Java and glibc programs that run here expect Linux
behaviour. The desktop can still offer case-insensitive matching on top
(Start's search already does).

`readdir` walks a directory's entries in hash order. To list a directory
in name order, the caller sorts the entries, as Files already does.

### File data

- **Inline:** a file of up to 2 KiB has its data in an `INLINE` item next
  to its inode. Reading it costs the one leaf read that found the inode.
  This covers configuration files, `DESKTOP.CFG` and most notes.
- **Extents:** larger files are a list of `EXTENT` items (a block pointer
  plus the number of bytes used, 40 bytes), each covering **at most
  64 KiB** (16 blocks). The limit is there for the checksum: a read
  anywhere in an extent has to read and verify all of it, so a 128 MiB
  extent would make every small read cost 128 MiB. Writing a file in one
  go still allocates one long run of blocks when free space allows, so
  the extents sit end to end and are read together (see
  [Reading fast](#reading-fast)).
- **Sparse:** a range with no extent reads as zeros and uses no space.
  The format supports it from v1; the write path creates holes only
  where something explicitly seeks past the end.

When a file grows or shrinks across 2 KiB it moves between inline and
extents.

## Free space

v1 keeps free space as a **bitmap**: one bit per block, 32 KiB of bitmap
per GiB of disk (1 MiB for a 32 GiB disk). The bitmap is stored in
4 KiB blocks, each covering 128 MiB of disk (bit *n* of bitmap block *k*
is block `k * 32768 + n`, 1 = in use; bits past the end of the volume
are set). The blocks are found through an index (`bitmap_root`): a small
tree of nodes with the usual header (tree 2) and up to 126 block
pointers each (almost 16 GiB of disk); level 1 points at bitmap blocks,
and another level is added when the disk outgrows one node. Like
everything else, it's copy-on-write.

- v1 reads the **whole bitmap into memory** at mount (16 KiB for the
  512 MiB volume), plus a second bitmap of blocks *held* for deferred
  frees (below). If disks get large enough for that to matter, the
  bitmap can be read as needed with a small summary (free blocks per
  16 MiB region) kept in memory instead.
- **Allocating** goes forward from a cursor (next-fit), looking for a
  free run big enough for the whole write so the file is one run; if
  there isn't one, the longest run there is, and so on.
- **Reserve:** file data may not use the last 64 free blocks or 1/256 of
  the volume, whichever is more. That space is left for the tree and
  bitmap blocks commits write, so a full disk still has room for the
  commit that deletes files to free space.
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

## Growing a volume

`make` builds a **512 MiB** volume. Nothing in the format depends on that
size, so a volume can grow up to the 64-bit limit later:

- Block pointers are 64-bit, and the tree doesn't depend on the disk
  size at all.
- Growing means one ordinary commit: write bitmap blocks for the new space
  (all free), adding an index level if needed; then the superblock with
  the larger `total_blocks`. A crash during it leaves the old size, just
  like any other commit.
- **On the host:** `tools/kfs.py grow disk.img 4G` enlarges the image
  file, then makes that commit.
- **In KonjacOS:** at mount, if the disk is larger than `total_blocks`
  (the image was enlarged, or QEMU was given a bigger disk), KFS asks
  before growing into the new space. Later it can grow automatically.

Shrinking isn't supported: it would mean moving live data out of the
space being removed, which is defragmentation (milestone 8).

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

**What v1 does:** a commit at the end of **every operation** (a file
written, a folder made, a rename, a delete) rather than on a timer. That
is simpler, needs no background thread, and makes every operation
durable when it returns, at about 4 ms each under QEMU. Two other
differences from the steps above:

- File data is written to its new blocks when the file is written,
  before the commit, so it doesn't wait in memory. It isn't reachable
  until the commit lands, so a crash just leaves those blocks free.
- If an operation fails part-way (the disk is full, a read error), its
  changes are dropped and the in-memory state is reloaded from the last
  commit. The operation either happened completely or not at all.

A long operation, such as deleting a big folder, commits part of the way
through once it has copied 128 tree nodes, after a step that leaves the
tree consistent (one entry removed with everything under it). A crash
then leaves some of the folder deleted, never a broken tree.

Batching several operations into one commit on a timer is the upgrade
if commits ever become the bottleneck.

## Memory

- **Node cache:** a fixed-size LRU of tree nodes, 1 MiB by default. Nodes
  near the root are touched on every lookup, so they stay in the cache
  naturally; a lookup in a tree with millions of files costs at most a
  few disk reads.
- **Dirty nodes** are pinned in memory until their commit. If they fill
  half the cache, a commit is started early, so memory use stays bounded.
- **Data:** reading a whole file goes straight from the disk into the
  caller's buffer. Small reads (`read_at`) go through an 8 MiB cache of
  whole, already-checked extents, keyed by block number and checksum,
  so an entry can never stand in for data written since and nothing
  needs invalidating.

### Reading fast

FAT16 was the bar to clear ([Removing FAT16](#removing-fat16)). What
KFS does:

- **Neighbouring extents are read together**, up to 256 KiB per disk
  request, and each is checked against its own checksum afterwards.
- **Several requests are in flight at once** (`block::read_many`): QEMU
  works on them in parallel, which nearly doubles throughput from an
  image on a slow host filesystem. Each extent is checked as soon as its
  request arrives, while the others are still on their way, so the
  checksums cost almost no time: the CPU would otherwise just be
  waiting.
- **The disk writes straight into the caller's buffer**: virtio-blk
  hands the device the buffer's physical pages instead of copying
  through a bounce buffer.
- **Small reads read ahead:** a `read_at` that misses the cache fetches
  the next 256 KiB along with what it needs, so reading a file in small
  pieces costs one disk request per 256 KiB instead of one per call.
- **Writes go out together:** a file's extents, and a commit's tree
  nodes and bitmap blocks, are written with several requests in flight.

`diskbench` (median of 5 runs, caches emptied before each, release
build, images on the WSL host's Windows drive):

| | KonjacFS | FAT16 |
|---|---:|---:|
| Read DOOM1.WAD (4 MB) | 9.0 ms | 13.7 ms |
| Read it in 4 KiB pieces | 31.1 ms | 160.1 ms |
| 256 random 4 KiB reads | 21.0 ms | 137.1 ms |
| ... again (cached) | 2.8 ms | 149.5 ms |
| Write 1 MiB | 6.5 ms | 37.1 ms |
| Read it back | 5.7 ms | 5.6 ms |
| List a folder x20 | 0.9 ms | 2.9 ms |
| Delete it | 2.1 ms | 1.2 ms |

Delete is slower because a KFS delete is durable when it returns (two
flushes around the superblock); FAT16 never flushes.

## How it fits into the kernel

Every part of the kernel goes through a small **VFS layer** (`vfs.rs`)
with the operations it uses (`read_file`, `open_file`/`read_at`,
`write_file`, `list_dir`, `stat_path`, `create_dir`, `rename`, `remove`,
`copy`), which dispatches by mount point:

- `/` is KFS when a KFS disk is present.
- The FAT16 disk stays available at `/fat` while KFS proves itself (see
  [Removing FAT16](#removing-fat16)). With no KFS disk, FAT16 is `/` as
  before.
- Moving between the two copies, then deletes the original. `/fat`
  itself can't be renamed or deleted.
- The first time KFS is `/`, `DESKTOP.CFG` (settings and pins) and
  `APEX.PWD` (the admin password's hash) are copied from `/fat` if KFS
  doesn't have them yet.
- `statfs` and Settings report the disk at `/`.

## Tools

`tools/kfs.py` (Python 3, standard library only, so `make` needs no new
packages). It's written from this document, separately from the kernel
code: if the two disagree about the format, the tests catch it.

- **`kfs.py mkfs DIR IMAGE --size 512M`** builds a KFS image from a
  folder, the way `mtools` builds the FAT16 image from `disk_root/`.
  `make kfs` runs it.
- **`kfs.py check IMAGE`** checks an image: every checksum, the tree's
  key order, the bitmap against what the trees reference, that every
  object has an inode and is in exactly one folder, that link counts,
  sizes and block counts add up, and that every directory entry sits at
  its name's hash. It exits with an error if anything is
  wrong; the tests run it after every scenario.
- **`kfs.py ls|cat|get`** reads files out of an image on the host.
- **`kfs.py put|mkdir|rm`** changes an image the way the kernel does:
  new data, a rebuilt tree and a rewritten bitmap go into blocks the
  current state doesn't use, then the next superblock makes them live,
  so an interrupted `put` leaves the image as it was. The Makefile's
  test-program targets use `put` to add their programs to `kfs.img`, as
  they use `mcopy` for the FAT16 disk.
- **`tools/crash_test.py`** and **`tools/bitrot_test.py`**: see
  [Testing](#testing).

## Testing

- **Format round-trip:** `kfs.py mkfs` builds an image, the kernel reads
  every file back and compares checksums.
- **Behaviour:** the Files/Notepad/DOOM scenarios from the FAT16 work,
  then `kfs.py check`.
- **`kfstest`** (a Terminal command) makes random new files (sizes
  either side of the inline limit and the extent size), overwrites,
  renames, moves, new folders and folder deletes, checking every file
  against what it should hold. `kfstest N SEED keep` leaves its folder
  for `kfs.py check`; `fill` then writes 1 MiB files until the disk is
  full, checks the failed write left nothing behind, deletes them and
  checks every block came back.
- **Crash testing** (`tools/crash_test.py`), which is what backs the
  "never corrupts" goal. It boots KonjacOS on a small KFS disk, starts
  `kfstest`, and kills QEMU (SIGKILL) at a random moment 0.1-4 s in,
  over and over on the same disk. After each crash the image must pass
  `kfs.py check`, the next boot must mount it, and every file kfstest
  wrote must be whole: each file of 16 bytes or more starts with
  `KFST`, a tag and its size, and the rest follows from the tag, so a
  mix of old and new contents is caught. QEMU writes go straight to the
  host's file, so this tests crashes at every point in the sequence of
  writes, but not a disk that loses or reorders writes it hadn't been
  told to flush; the commit order (flush before and after the
  superblock) is what covers that.
- **Bit rot** (`tools/bitrot_test.py`): flip one random byte in a random
  block of a fresh image (a superblock slot, the tree root, another tree
  node, a file's data, or the bitmap), check `kfs.py check` notices,
  then boot it and run `verify`, which reads every file. A file's data:
  exactly that file fails. A tree node: some files fail, the rest read.
  The root or the bitmap: the mount falls back to the previous commit or
  refuses, saying the disk is damaged. A superblock slot: nothing is
  lost. Never wrong data, never a crash.

## Milestones

Each one ends in something that boots and passes its tests.

1. **Read-only KFS** (done). `kfs.py`, the VFS layer, and a kernel that
   mounts a KFS disk read-only at `/kfs` and reads it. FAT16 stays `/`
   until 3.
2. **Writing** (done). Commits, the allocator, deferred frees, inline
   data and extents. Files and Notepad work on KFS.
3. **Crash-tested, then default** (done). Crash and bit-rot testing pass,
   `make` builds a 512 MiB KFS disk, FAT16 moves to `/fat`. Released as
   a new version.
4. **Remove FAT16**, once the checklist under
   [Removing FAT16](#removing-fat16) is met.
5. **Snapshots** (format already ready: `snap_root`, `birth_txg`).
6. **Reflinks and sparse writes** (instant copies in Files).
7. **LZ4 compression** (the `compression` byte in block pointers).
8. **Scrubbing, defragmentation and shrinking.**
9. **Direct I/O and a page cache**, once there are DMA drivers worth
   bypassing a cache for.

### Removing FAT16

FAT16 goes once KFS has shown it's better *and* works. The checklist:

- **Safe:** the crash and bit-rot tests pass, hundreds of runs each, with
  no failures.
- **At least as fast:** every `diskbench` line on KFS is as fast as FAT16
  or faster. *Met except for delete* (2.1 against 1.2 ms), which costs
  more only because a KFS delete is durable when it returns and FAT16's
  isn't; see [Reading fast](#reading-fast).
- **Everything works:** booting, the shell, Files, Notepad, DOOM, Java
  and the Linux programs, on KFS alone, for a full release.
- **Files can still get in and out:** `kfs.py` covers
  everything `mtools` did (adding files to an image, reading them back).

Then `fat16.rs`, the `/fat` mount and the `mtools` build steps are
deleted.

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
| Extents instead of cluster chains | v1: `EXTENT` items, up to 64 KiB each, laid end to end |
| 64-bit block addressing | v1 |
| Copy-on-write + atomic superblock switch | v1: [Commits](#commits), 8-slot superblock ring |
| Small files inside the index record | v1: `INLINE`, up to 2 KiB |
| Checksums in parent pointers (Merkle tree) | v1: xxHash64 in every block pointer |
| Bounded LRU cache / "on-demand node paging" | v1: one 1 MiB node cache |
| Sequential write buffer | v1: transaction groups (5 s / 4 MiB) |
| Instant snapshots | Milestone 5; format ready in v1 |
| Block cloning (reflinks) | Milestone 6 |
| Sparse files | Format in v1, write path in milestone 6 |
| Inline compression | Milestone 7; LZ4 first (fast, simple), Zstd maybe later |
| Background scrubbing | Milestone 8 |
| CoW aging / defragmentation | Milestone 8; reduced up front by allocating whole runs |
| Direct I/O for the JVM | Milestone 9: needs a page cache to bypass first |
| "A few KB of RAM at any size" | Replaced by a fixed, configurable cache (1 MiB); a few KB would make every lookup a disk read |
| BLAKE3 | xxHash64 instead: detecting bit rot doesn't need a cryptographic hash, and BLAKE3 costs far more CPU under QEMU |

## Decisions

1. **Case-sensitive names**, like Linux.
2. **512 MiB** default volume, growable later
   ([Growing a volume](#growing-a-volume)).
3. **FAT16 is removed** once KFS meets the
   [checklist](#removing-fat16); until then it stays at `/fat`.
