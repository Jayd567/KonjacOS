#!/usr/bin/env python3
"""KonjacFS (KFS) on the host: build an image, look inside one, check one,
change one.

    kfs.py mkfs SOURCE_DIR IMAGE [--size 512M] [--label NAME]
    kfs.py ls IMAGE [PATH]
    kfs.py cat IMAGE PATH
    kfs.py get IMAGE PATH DEST
    kfs.py check IMAGE
    kfs.py put IMAGE SOURCE [DEST]   a file or folder; DEST may be a folder
    kfs.py mkdir IMAGE PATH
    kfs.py rm IMAGE PATH             a file, or a folder and all in it

The format is described in docs/kfs-design.md. This file is written from
that document, separately from the kernel's `kfs.rs`, so the two check
each other: an image this builds has to read back correctly in KonjacOS,
and `check` verifies every checksum, the tree's ordering and the
free-space bitmap. `put`, `mkdir` and `rm` change an image with a
copy-on-write commit, as the kernel does. Standard library only.
"""

import argparse
import os
import struct
import sys
import time
import uuid as uuidlib

BLOCK = 4096
SB_FIRST, SB_SLOTS = 1, 8
DATA_START = 16
MAGIC_SB = b"KONJACFS"
MAGIC_NODE = b"KFSN"
VERSION = 1

TREE_FS, TREE_BITMAP = 1, 2
BP_NODE, BP_BITMAP_INDEX, BP_BITMAP, BP_DATA = 1, 2, 3, 4
K_INODE, K_DIR_ENTRY, K_INLINE, K_EXTENT = 1, 2, 3, 4
S_IFDIR, S_IFREG = 0o040000, 0o100000
T_FILE, T_DIR = 1, 2
ROOT_OBJECT = 1

HEADER = 64
KEY = 24
BP = 32
ITEM_HEADER = 24
INTERIOR_ENTRY = KEY + BP
INTERIOR_MAX = (BLOCK - HEADER) // INTERIOR_ENTRY  # 72
INDEX_MAX = (BLOCK - HEADER) // BP  # 126
BITS_PER_BITMAP = BLOCK * 8  # 32768 blocks = 128 MiB
INLINE_MAX = 2048
EXTENT_MAX_BLOCKS = 16  # 64 KiB: one checksum, one request
INODE_SIZE = 128

# --- xxHash64 (seed 0) --------------------------------------------------------

P1, P2, P3, P4, P5 = (
    11400714785074694791,
    14029467366897019727,
    1609587929392839161,
    9650029242287828579,
    2870177450012600261,
)
M64 = (1 << 64) - 1


def _rotl(x, r):
    return ((x << r) | (x >> (64 - r))) & M64


def _round(acc, lane):
    acc = (acc + lane * P2) & M64
    return (_rotl(acc, 31) * P1) & M64


def _merge(acc, v):
    acc ^= _round(0, v)
    return (acc * P1 + P4) & M64


def xxh64(data, seed=0):
    n = len(data)
    i = 0
    if n >= 32:
        v1 = (seed + P1 + P2) & M64
        v2 = (seed + P2) & M64
        v3 = seed
        v4 = (seed - P1) & M64
        limit = n - n % 32
        for a, b, c, d in struct.iter_unpack("<4Q", memoryview(data)[:limit]):
            v1 = _round(v1, a)
            v2 = _round(v2, b)
            v3 = _round(v3, c)
            v4 = _round(v4, d)
        i = limit
        h = (_rotl(v1, 1) + _rotl(v2, 7) + _rotl(v3, 12) + _rotl(v4, 18)) & M64
        for v in (v1, v2, v3, v4):
            h = _merge(h, v)
    else:
        h = (seed + P5) & M64
    h = (h + n) & M64
    while i + 8 <= n:
        (k,) = struct.unpack_from("<Q", data, i)
        h ^= _round(0, k)
        h = (_rotl(h, 27) * P1 + P4) & M64
        i += 8
    if i + 4 <= n:
        (k,) = struct.unpack_from("<I", data, i)
        h ^= (k * P1) & M64
        h = (_rotl(h, 23) * P2 + P3) & M64
        i += 4
    while i < n:
        h ^= (data[i] * P5) & M64
        h = (_rotl(h, 11) * P1) & M64
        i += 1
    h ^= h >> 33
    h = (h * P2) & M64
    h ^= h >> 29
    h = (h * P3) & M64
    h ^= h >> 32
    return h


# --- Structures -----------------------------------------------------------------


def pack_bp(block, count, kind, birth, checksum):
    return struct.pack("<QIBBHQQ", block, count, kind, 0, 0, birth, checksum)


def unpack_bp(b, off=0):
    block, count, kind, comp, _r, birth, checksum = struct.unpack_from("<QIBBHQQ", b, off)
    return {"block": block, "count": count, "kind": kind, "compression": comp, "birth": birth, "checksum": checksum}


def pack_key(obj, kind, offset):
    return struct.pack("<QQB7x", obj, offset, kind)


def unpack_key(b, off=0):
    obj, offset, kind = struct.unpack_from("<QQB", b, off)
    return (obj, kind, offset)


def node_header(level, tree, count, birth):
    return MAGIC_NODE + struct.pack("<BBHQ", level, tree, count, birth) + bytes(HEADER - 16)


def name_hash(name):
    """A directory entry's key offset: the name's hash with the low three
    bits clear, leaving room for up to seven colliding names after it."""
    return xxh64(name) & ~7 & M64


def pack_inode(mode, size, links, mtime_ns, blocks, flags=0):
    b = struct.pack("<IIQII4QQ", mode, flags, size, links, 0, mtime_ns, mtime_ns, mtime_ns, mtime_ns, blocks)
    return b + bytes(INODE_SIZE - len(b))


def unpack_inode(b):
    mode, flags, size, links, gen, c, m, ch, a, blocks = struct.unpack_from("<IIQII4QQ", b)
    return {"mode": mode, "flags": flags, "size": size, "links": links, "modified": m, "blocks": blocks}


def parse_size(s):
    s = s.strip().upper()
    mult = 1
    for suffix, m in (("K", 1 << 10), ("M", 1 << 20), ("G", 1 << 30), ("T", 1 << 40)):
        if s.endswith(suffix):
            mult, s = m, s[: -len(suffix)]
            break
    return int(s) * mult


# --- mkfs -------------------------------------------------------------------------


class Builder:
    def __init__(self, total_blocks):
        self.total = total_blocks
        self.next = DATA_START
        self.blocks = {}  # block number -> 4096 bytes

    def alloc(self, n):
        b = self.next
        self.next += n
        if self.next > self.total:
            sys.exit("kfs: the files don't fit in the volume; use a larger --size")
        return b

    def put(self, block, data):
        assert len(data) == BLOCK
        self.blocks[block] = data


def set_bits(bits, lo, hi, value=1):
    """Marks blocks lo..hi used (or free) in a bitmap held as a bytearray."""
    for blk in range(lo, hi):
        if value:
            bits[blk >> 3] |= 1 << (blk & 7)
        else:
            bits[blk >> 3] &= ~(1 << (blk & 7)) & 255


def write_tree(items, alloc, put, txg):
    """Writes a filesystem tree holding `items` (sorted `(key, data)`):
    leaves packed full, then interior levels. Each node goes in a block
    from `alloc()`, written with `put(block, data)`. Returns `(root
    pointer, depth)`."""
    leaves = []
    cur = []
    used = HEADER
    for it in items:
        need = ITEM_HEADER + len(it[1])
        if cur and used + need > BLOCK:
            leaves.append(cur)
            cur, used = [], HEADER
        cur.append(it)
        used += need
    leaves.append(cur)

    def write_leaf(its):
        blk = alloc()
        out = bytearray(BLOCK)
        out[0:HEADER] = node_header(0, TREE_FS, len(its), txg)
        end = BLOCK
        for i, ((obj, kind, off), data) in enumerate(its):
            end -= len(data)
            out[end:end + len(data)] = data
            h = HEADER + i * ITEM_HEADER
            out[h:h + ITEM_HEADER] = struct.pack("<QQBBHHH", obj, off, kind, 0, 0, end, len(data))
        out = bytes(out)
        put(blk, out)
        return (its[0][0] if its else (0, 0, 0)), pack_bp(blk, 1, BP_NODE, txg, xxh64(out))

    level = [write_leaf(l) for l in leaves]
    depth = 0
    while len(level) > 1:
        depth += 1
        up = []
        for i in range(0, len(level), INTERIOR_MAX):
            group = level[i:i + INTERIOR_MAX]
            blk = alloc()
            out = bytearray(BLOCK)
            out[0:HEADER] = node_header(depth, TREE_FS, len(group), txg)
            for j, (key, bp) in enumerate(group):
                o = HEADER + j * INTERIOR_ENTRY
                out[o:o + KEY] = pack_key(key[0], key[1], key[2])
                out[o + KEY:o + INTERIOR_ENTRY] = bp
            out = bytes(out)
            put(blk, out)
            up.append((group[0][0], pack_bp(blk, 1, BP_NODE, txg, xxh64(out))))
        level = up
    return level[0][1], depth


def index_nodes(nbitmap):
    """How many nodes the bitmap index needs for `nbitmap` bitmap blocks."""
    total, n = 0, nbitmap
    while True:
        n = -(-n // INDEX_MAX)
        total += n
        if n == 1:
            return total


def write_bitmap(bits, alloc, put, txg):
    """Writes the bitmap `bits` (a bytearray, a whole number of blocks)
    and its index, bitmap blocks first, each in a block from `alloc()`.
    Returns the index's root pointer."""
    level = []
    for i in range(len(bits) // BLOCK):
        out = bytes(bits[i * BLOCK:(i + 1) * BLOCK])
        blk = alloc()
        put(blk, out)
        level.append(pack_bp(blk, 1, BP_BITMAP, txg, xxh64(out)))
    lvl = 0
    while True:
        lvl += 1
        up = []
        for i in range(0, len(level), INDEX_MAX):
            group = level[i:i + INDEX_MAX]
            out = bytearray(BLOCK)
            out[0:HEADER] = node_header(lvl, TREE_BITMAP, len(group), txg)
            for j, bp in enumerate(group):
                out[HEADER + j * BP:HEADER + (j + 1) * BP] = bp
            out = bytes(out)
            blk = alloc()
            put(blk, out)
            up.append(pack_bp(blk, 1, BP_BITMAP_INDEX, txg, xxh64(out)))
        level = up
        if len(level) == 1:
            return level[0]


def mkfs(src, image, size, label):
    total = size // BLOCK
    if total < 1024:
        sys.exit("kfs: the volume must be at least 4 MiB")
    b = Builder(total)
    txg = 1

    # The bitmap's blocks and index come first, so every block in use ends
    # up in one run from 0 to wherever allocation stops.
    nbitmap = -(-total // BITS_PER_BITMAP)
    bitmap_first = b.alloc(nbitmap + index_nodes(nbitmap))

    # Objects: walk the tree, root first.
    objects = []  # (object, path on host, is_dir, parent object, name)
    def walk(host, obj):
        entries = sorted(os.listdir(host))
        children = []
        for name in entries:
            p = os.path.join(host, name)
            child = len(objects) + 1
            objects.append((child, p, os.path.isdir(p), obj, name.encode()))
            children.append((child, p))
        for child, p in children:
            if os.path.isdir(p):
                walk(p, child)
    objects.append((ROOT_OBJECT, src, True, 0, b""))
    walk(src, ROOT_OBJECT)

    items = []  # (key tuple, data)
    by_parent = {}
    for obj, path, is_dir, parent, name in objects:
        by_parent.setdefault(parent, []).append((obj, is_dir, name))
    for obj, path, is_dir, parent, name in objects:
        mtime = int(os.stat(path).st_mtime * 1e9)
        if is_dir:
            subdirs = sum(1 for _, d, _ in by_parent.get(obj, []) if d)
            items.append(((obj, K_INODE, 0), pack_inode(S_IFDIR | 0o755, 0, 2 + subdirs, mtime, 0)))
            used = set()
            for child, child_dir, child_name in by_parent.get(obj, []):
                if len(child_name) > 255:
                    sys.exit(f"kfs: name too long: {child_name!r}")
                base = name_hash(child_name)
                off = next((base + k for k in range(8) if base + k not in used), None)
                if off is None:
                    sys.exit("kfs: too many names with the same hash in one folder")
                used.add(off)
                data = struct.pack("<QBB", child, T_DIR if child_dir else T_FILE, len(child_name)) + child_name
                items.append(((obj, K_DIR_ENTRY, off), data))
        else:
            with open(path, "rb") as f:
                content = f.read()
            if len(content) <= INLINE_MAX:
                items.append(((obj, K_INODE, 0), pack_inode(S_IFREG | 0o644, len(content), 1, mtime, 0)))
                items.append(((obj, K_INLINE, 0), content))
            else:
                nblocks = -(-len(content) // BLOCK)
                first = b.alloc(nblocks)
                items.append(((obj, K_INODE, 0), pack_inode(S_IFREG | 0o644, len(content), 1, mtime, nblocks)))
                for k in range(0, nblocks, EXTENT_MAX_BLOCKS):
                    count = min(EXTENT_MAX_BLOCKS, nblocks - k)
                    chunk = content[k * BLOCK:(k + count) * BLOCK]
                    chunk += bytes(count * BLOCK - len(chunk))
                    for j in range(count):
                        b.put(first + k + j, chunk[j * BLOCK:(j + 1) * BLOCK])
                    bp = pack_bp(first + k, count, BP_DATA, txg, xxh64(chunk))
                    length = min(count * BLOCK, len(content) - k * BLOCK)
                    items.append(((obj, K_EXTENT, k * BLOCK), bp + struct.pack("<Q", length)))
    items.sort(key=lambda it: it[0])

    fs_root, depth = write_tree(items, lambda: b.alloc(1), b.put, txg)

    # Bitmap: every block from 0 up to the allocation point is in use, and
    # so are the bits past the end of the volume, so nothing allocates them.
    used_blocks = b.next
    bits = bytearray(nbitmap * BLOCK)
    set_bits(bits, 0, used_blocks)
    set_bits(bits, total, nbitmap * BITS_PER_BITMAP)
    blocks = iter(range(bitmap_first, bitmap_first + nbitmap + index_nodes(nbitmap)))
    bitmap_root = write_bitmap(bits, lambda: next(blocks), b.put, txg)

    sb = bytearray(BLOCK)
    struct.pack_into("<8sIIQQQ", sb, 0, MAGIC_SB, VERSION, BLOCK, total, txg, 0)
    sb[40:72] = fs_root
    sb[72:104] = bitmap_root
    sb[104:136] = bytes(32)
    struct.pack_into("<QQ", sb, 136, total - used_blocks, len(objects) + 1)
    sb[152:168] = uuidlib.uuid4().bytes
    lab = label.encode()[:32]
    sb[168:168 + len(lab)] = lab
    struct.pack_into("<Q", sb, BLOCK - 8, xxh64(bytes(sb[:BLOCK - 8])))
    # Every slot, so a fresh image survives a damaged superblock. Commits
    # replace them one by one.
    for slot in range(SB_SLOTS):
        b.put(SB_FIRST + slot, bytes(sb))

    with open(image, "wb") as f:
        f.truncate(total * BLOCK)
        for blkno in sorted(b.blocks):
            f.seek(blkno * BLOCK)
            f.write(b.blocks[blkno])
    files = sum(1 for o in objects if not o[2])
    dirs = len(objects) - files
    print(f"kfs: {image}: {size >> 20} MiB, {files} files, {dirs} folders, "
          f"{used_blocks} of {total} blocks used, tree depth {depth + 1}")


# --- Reading ------------------------------------------------------------------------


class Image:
    def __init__(self, path):
        self.f = open(path, "rb")
        self.f.seek(0, 2)
        self.size = self.f.tell()
        self.sb = self.superblock()

    def raw(self, block, count=1):
        self.f.seek(block * BLOCK)
        data = self.f.read(count * BLOCK)
        if len(data) != count * BLOCK:
            raise IOError(f"block {block} is past the end of the image")
        return data

    def superblock(self):
        best = None
        for slot in range(SB_SLOTS):
            b = self.raw(SB_FIRST + slot)
            if b[:8] != MAGIC_SB:
                continue
            (cks,) = struct.unpack_from("<Q", b, BLOCK - 8)
            if cks != xxh64(b[:BLOCK - 8]):
                continue
            magic, version, bs, total, txg, features = struct.unpack_from("<8sIIQQQ", b, 0)
            if best is None or txg > best["txg"]:
                free, next_obj = struct.unpack_from("<QQ", b, 136)
                best = {"slot": slot, "version": version, "total": total, "txg": txg, "features": features,
                        "fs_root": unpack_bp(b, 40), "bitmap_root": unpack_bp(b, 72), "free": free,
                        "next_object": next_obj, "label": b[168:200].rstrip(b"\0").decode(errors="replace")}
        if best is None:
            sys.exit("kfs: no valid superblock: not a KFS image, or damaged")
        if best["version"] != VERSION:
            sys.exit(f"kfs: unsupported version {best['version']}")
        return best

    def read_bp(self, bp):
        data = self.raw(bp["block"], bp["count"])
        if xxh64(data) != bp["checksum"]:
            raise IOError(f"checksum mismatch in block {bp['block']} (+{bp['count']})")
        return data

    def node(self, bp):
        b = self.read_bp(bp)
        if b[:4] != MAGIC_NODE:
            raise IOError(f"block {bp['block']} isn't a tree node")
        level, tree, count, birth = struct.unpack_from("<BBHQ", b, 4)
        return b, level, tree, count

    def items(self, bp=None):
        """Every (key, data) in the filesystem tree, in order."""
        bp = bp or self.sb["fs_root"]
        b, level, tree, count = self.node(bp)
        if level == 0:
            for i in range(count):
                h = HEADER + i * ITEM_HEADER
                obj, off, kind, _p, _q, doff, dlen = struct.unpack_from("<QQBBHHH", b, h)
                yield (obj, kind, off), b[doff:doff + dlen]
        else:
            for i in range(count):
                o = HEADER + i * INTERIOR_ENTRY
                yield from self.items(unpack_bp(b, o + KEY))

    def object_items(self, obj):
        return [(k, d) for k, d in self.items() if k[0] == obj]

    def lookup(self, path):
        obj = ROOT_OBJECT
        for part in [p for p in path.split("/") if p]:
            name = part.encode()
            found = None
            for (o, kind, off), d in self.object_items(obj):
                if kind == K_DIR_ENTRY:
                    child, t, n = struct.unpack_from("<QBB", d)
                    if d[10:10 + n] == name:
                        found = child
            if found is None:
                sys.exit(f"kfs: {path}: no such file or folder")
            obj = found
        return obj

    def inode(self, obj):
        for (o, kind, off), d in self.object_items(obj):
            if kind == K_INODE:
                return unpack_inode(d)
        raise IOError(f"object {obj} has no inode")

    def contents(self, obj):
        ino = self.inode(obj)
        out = bytearray(ino["size"])
        for (o, kind, off), d in self.object_items(obj):
            if kind == K_INLINE:
                out[0:len(d)] = d
            elif kind == K_EXTENT:
                bp = unpack_bp(d)
                (length,) = struct.unpack_from("<Q", d, BP)
                out[off:off + length] = self.read_bp(bp)[:length]
        return bytes(out)

    def listing(self, obj):
        out = []
        for (o, kind, off), d in self.object_items(obj):
            if kind == K_DIR_ENTRY:
                child, t, n = struct.unpack_from("<QBB", d)
                out.append((d[10:10 + n].decode(errors="replace"), child, t))
        return sorted(out)


def check_objects(items, sb):
    """Every object is whole and in exactly one folder (v1 has no hard
    links), link counts and sizes add up, and entries sit at their hash."""
    problems = []
    inodes, entries, inline, extents, owners = {}, {}, {}, {}, {}
    for (obj, kind, off), d in items:
        if kind == K_INODE:
            inodes[obj] = unpack_inode(d)
        elif kind == K_DIR_ENTRY:
            child, t, n = struct.unpack_from("<QBB", d)
            name = d[10:10 + n]
            entries.setdefault(obj, []).append((name, child, t))
            owners.setdefault(child, []).append(obj)
            if not 0 <= off - name_hash(name) < 8:
                problems.append(f"folder {obj}: entry {name!r} isn't at its name's hash")
        elif kind == K_INLINE:
            inline[obj] = len(d)
        elif kind == K_EXTENT:
            bp = unpack_bp(d)
            (length,) = struct.unpack_from("<Q", d, BP)
            extents.setdefault(obj, []).append((off, length, bp))
        else:
            problems.append(f"object {obj} has an item of unknown kind {kind}")
    if ROOT_OBJECT not in inodes:
        problems.append("no root folder")
    everything = set(inodes) | set(entries) | set(inline) | set(extents) | set(owners)
    for obj in sorted(everything):
        ino = inodes.get(obj)
        if ino is None:
            where = owners.get(obj)
            problems.append(f"object {obj} has no inode" + (f" (named in folder {where[0]})" if where else ""))
            continue
        if obj >= sb["next_object"]:
            problems.append(f"object {obj} is at or past next_object ({sb['next_object']})")
        held_by = owners.get(obj, [])
        if obj != ROOT_OBJECT and len(held_by) != 1:
            problems.append(f"object {obj} is in {len(held_by)} folders (should be 1)")
        is_dir = ino["mode"] & 0o170000 == S_IFDIR
        if is_dir:
            kids = entries.get(obj, [])
            subdirs = sum(1 for _, _, t in kids if t == T_DIR)
            if ino["links"] != 2 + subdirs:
                problems.append(f"folder {obj} has link count {ino['links']}, expected {2 + subdirs}")
            for name, child, t in kids:
                cino = inodes.get(child)
                if cino and (t == T_DIR) != (cino["mode"] & 0o170000 == S_IFDIR):
                    problems.append(f"folder {obj}: entry {name!r} has the wrong type")
            if obj in inline or obj in extents:
                problems.append(f"folder {obj} has file data")
        else:
            if obj in entries:
                problems.append(f"file {obj} has folder entries")
            if obj in inline and obj in extents:
                problems.append(f"file {obj} has both inline data and extents")
            if obj in inline and inline[obj] != ino["size"]:
                problems.append(f"file {obj}: inline data is {inline[obj]} bytes, size says {ino['size']}")
            blocks = 0
            for off, length, bp in extents.get(obj, []):
                blocks += bp["count"]
                if off % BLOCK or length > bp["count"] * BLOCK or bp["count"] > EXTENT_MAX_BLOCKS:
                    problems.append(f"file {obj}: bad extent at {off}")
                if off + length > ino["size"]:
                    problems.append(f"file {obj}: extent at {off} runs past its size {ino['size']}")
            if blocks != ino["blocks"]:
                problems.append(f"file {obj}: {blocks} blocks in extents, the inode says {ino['blocks']}")
            if obj not in inline and obj not in extents and ino["size"]:
                problems.append(f"file {obj} has size {ino['size']} but no data")
    return problems


def check_image(path):
    """`(superblock, problems, object count)` for the image at `path`."""
    img = Image(path)
    sb = img.sb
    problems = []
    referenced = set(range(0, DATA_START))

    def mark(bp):
        for blk in range(bp["block"], bp["block"] + bp["count"]):
            if blk in referenced:
                problems.append(f"block {blk} is referenced twice")
            referenced.add(blk)

    # Filesystem tree: checksums, ordering, node shapes.
    last = [None]
    unreadable = [False]
    def walk(bp, depth):
        mark(bp)
        try:
            b, level, tree, count = img.node(bp)
        except IOError as e:
            problems.append(str(e))
            unreadable[0] = True
            return
        if tree != TREE_FS:
            problems.append(f"node {bp['block']} belongs to tree {tree}")
        if level == 0:
            for i in range(count):
                obj, off, kind, _p, _q, doff, dlen = struct.unpack_from("<QQBBHHH", b, HEADER + i * ITEM_HEADER)
                key = (obj, kind, off)
                if last[0] is not None and key <= last[0]:
                    problems.append(f"keys out of order at {key}")
                last[0] = key
                if doff < HEADER + count * ITEM_HEADER or doff + dlen > BLOCK:
                    problems.append(f"item {key} data out of bounds")
                if kind == K_EXTENT:
                    xbp = unpack_bp(b, doff)
                    mark(xbp)
                    try:
                        img.read_bp(xbp)
                    except IOError as e:
                        problems.append(str(e))
        else:
            for i in range(count):
                walk(unpack_bp(b, HEADER + i * INTERIOR_ENTRY + KEY), depth + 1)
    walk(sb["fs_root"], 0)

    # Bitmap index and bitmap.
    bitmap = []
    def walk_index(bp):
        mark(bp)
        b, level, tree, count = img.node(bp)
        for i in range(count):
            child = unpack_bp(b, HEADER + i * BP)
            if level == 1:
                mark(child)
                bitmap.append(img.read_bp(child))
            else:
                walk_index(child)
    try:
        walk_index(sb["bitmap_root"])
    except IOError as e:
        problems.append(str(e))
    total = sb["total"]
    used = 0
    if unreadable[0]:
        # Whatever hangs off an unreadable node looks unreferenced; don't
        # bury the real problem under that.
        problems.append("part of the tree is unreadable, so the bitmap wasn't cross-checked")
        total = 0
    for blk in range(total):
        word = bitmap[blk // BITS_PER_BITMAP] if blk // BITS_PER_BITMAP < len(bitmap) else None
        bit = word is not None and word[(blk % BITS_PER_BITMAP) >> 3] >> (blk & 7) & 1
        if bit:
            used += 1
        if bit and blk not in referenced:
            problems.append(f"block {blk} is marked used but nothing references it")
        if not bit and blk in referenced:
            problems.append(f"block {blk} is referenced but marked free")
        if len(problems) > 50:
            break
    if not unreadable[0] and total - used != sb["free"]:
        problems.append(f"superblock says {sb['free']} free blocks, the bitmap says {total - used}")

    # Objects: every directory entry points at an inode. (Skipped if the
    # tree itself is damaged: the walk above has already said where.)
    inodes = set()
    try:
        items = list(img.items())
    except IOError:
        items = None
    if items is not None:
        problems += check_objects(items, sb)
        inodes = {k[0] for k, d in items if k[1] == K_INODE}
    return sb, problems, len(inodes)


def check(path):
    sb, problems, objects = check_image(path)
    print(f"kfs: {path}: txg {sb['txg']} (slot {sb['slot']}), {sb['total']} blocks, {sb['free']} free, "
          f"{objects} objects, label {sb['label']!r}")
    for p in problems[:50]:
        print("  PROBLEM:", p)
    if problems:
        sys.exit(1)
    print("kfs: no problems found")


# --- Changing an image ----------------------------------------------------------------

INODE_FMT = "<IIQII4QQ"
RESERVE_MIN = 64


class Volume:
    """An image opened for changes. They collect in memory; `commit()`
    writes them copy-on-write, the way the kernel does: new file data, a
    rebuilt tree and a rewritten bitmap go into blocks the current state
    doesn't use, then the next superblock in the ring makes them live.
    Nothing the current state uses is overwritten, so an interrupted
    commit leaves the image as it was."""

    def __init__(self, path):
        self.img = Image(path)
        sb = self.img.sb
        self.raw_sb = bytearray(self.img.raw(SB_FIRST + sb["slot"]))
        self.total = sb["total"]
        self.txg = sb["txg"] + 1
        self.next_object = sb["next_object"]
        self.items = {}
        self.keys = {}  # object -> its keys
        for k, d in self.img.items():
            self.set(k, d)
        # What the current state uses, freed once the new state is written.
        self.frees = []
        self.bits = bytearray()
        self._collect_tree(sb["fs_root"])
        self._collect_bitmap(sb["bitmap_root"])
        self.f = open(path, "r+b")
        self.changed = False

    def _collect_tree(self, bp):
        self.frees.append(bp)
        b, level, tree, count = self.img.node(bp)
        if level:
            for i in range(count):
                self._collect_tree(unpack_bp(b, HEADER + i * INTERIOR_ENTRY + KEY))

    def _collect_bitmap(self, bp):
        self.frees.append(bp)
        b, level, tree, count = self.img.node(bp)
        for i in range(count):
            child = unpack_bp(b, HEADER + i * BP)
            if level == 1:
                self.frees.append(child)
                self.bits += self.img.read_bp(child)
            else:
                self._collect_bitmap(child)

    # Items, indexed by object.

    def set(self, key, data):
        self.items[key] = bytes(data)
        self.keys.setdefault(key[0], set()).add(key)
        self.changed = True

    def delete(self, key):
        del self.items[key]
        self.keys[key[0]].discard(key)
        self.changed = True

    def of(self, obj, kind=None):
        return sorted(k for k in self.keys.get(obj, ()) if kind is None or k[1] == kind)

    def inode(self, obj):
        return list(struct.unpack_from(INODE_FMT, self.items[(obj, K_INODE, 0)]))

    def put_inode(self, obj, fields):
        b = struct.pack(INODE_FMT, *fields)
        self.set((obj, K_INODE, 0), b + bytes(INODE_SIZE - len(b)))

    # Space.

    def free_count(self):
        used = sum(bin(x).count("1") for x in self.bits)
        return self.total - (used - (len(self.bits) * 8 - self.total))

    def alloc(self, want, reserve=0):
        """Marks used and returns `(start, n)`: the first run of `want`
        free blocks, else the longest run there is, leaving `reserve`."""
        room = self.free_count() - reserve
        if room <= 0:
            sys.exit("kfs: the image is full; use a larger --size")
        want = min(want, room)
        best, start, run = None, 0, 0
        blk = DATA_START
        while blk < self.total:
            if blk & 7 == 0 and self.bits[blk >> 3] == 255:
                run = 0
                blk += 8
                continue
            if not self.bits[blk >> 3] >> (blk & 7) & 1:
                if run == 0:
                    start = blk
                run += 1
                if best is None or run > best[1]:
                    best = (start, run)
                if run == want:
                    break
            else:
                run = 0
            blk += 1
        if best is None:
            sys.exit("kfs: the image is full; use a larger --size")
        set_bits(self.bits, best[0], best[0] + best[1])
        return best

    def alloc1(self):
        return self.alloc(1)[0]

    def write(self, block, data):
        self.f.seek(block * BLOCK)
        self.f.write(data)

    # Names.

    def entries(self, d):
        """`(name, child, is_dir, key)` for everything in folder `d`."""
        out = []
        for k in self.of(d, K_DIR_ENTRY):
            data = self.items[k]
            child, t, n = struct.unpack_from("<QBB", data)
            out.append((data[10:10 + n].decode(errors="replace"), child, t == T_DIR, k))
        return out

    def lookup(self, path):
        """`(object, is_dir)` at `path`, or None."""
        cur = (ROOT_OBJECT, True)
        for part in [x for x in path.split("/") if x]:
            if not cur[1]:
                return None
            found = [(c, d) for n, c, d, k in self.entries(cur[0]) if n == part]
            if not found:
                return None
            cur = found[0]
        return cur

    def parent(self, path):
        """The folder `path` goes in (made if missing), and its name."""
        parts = [x for x in path.split("/") if x]
        if not parts:
            sys.exit("kfs: that's the root folder")
        name = parts[-1]
        if len(name.encode()) > 255 or name in (".", ".."):
            sys.exit(f"kfs: invalid name {name!r}")
        return self.mkdir("/" + "/".join(parts[:-1])), name

    def touch(self, d, links=0):
        f = self.inode(d)
        f[3] += links
        f[7] = f[8] = time.time_ns()
        self.put_inode(d, f)

    def add_entry(self, d, name, child, is_dir):
        raw = name.encode()
        base = name_hash(raw)
        used = {k[2] for k in self.of(d, K_DIR_ENTRY)}
        off = next((base + j for j in range(8) if base + j not in used), None)
        if off is None:
            sys.exit(f"kfs: too many names like {name!r} in one folder")
        self.set((d, K_DIR_ENTRY, off), struct.pack("<QBB", child, T_DIR if is_dir else T_FILE, len(raw)) + raw)

    def new_object(self):
        obj = self.next_object
        self.next_object += 1
        return obj

    # Changes.

    def mkdir(self, path):
        """The folder at `path`, made (with any missing parents) if needed."""
        found = self.lookup(path)
        if found:
            if not found[1]:
                sys.exit(f"kfs: {path} is a file")
            return found[0]
        d, name = self.parent(path)
        obj = self.new_object()
        self.put_inode(obj, [S_IFDIR | 0o755, 0, 0, 2, 0] + [time.time_ns()] * 4 + [0])
        self.add_entry(d, name, obj, True)
        self.touch(d, 1)
        return obj

    def drop_data(self, obj):
        for k in self.of(obj):
            if k[1] == K_EXTENT:
                self.frees.append(unpack_bp(self.items[k]))
            if k[1] in (K_INLINE, K_EXTENT):
                self.delete(k)

    def write_data(self, obj, data):
        """Stores `data` as `obj`'s contents; returns the blocks used."""
        if len(data) <= INLINE_MAX:
            self.set((obj, K_INLINE, 0), data)
            return 0
        nblocks = -(-len(data) // BLOCK)
        reserve = max(RESERVE_MIN, self.total // 256)
        done = 0
        while done < nblocks:
            start, got = self.alloc(nblocks - done, reserve)
            for k in range(0, got, EXTENT_MAX_BLOCKS):
                count = min(EXTENT_MAX_BLOCKS, got - k)
                at = (done + k) * BLOCK
                chunk = data[at:at + count * BLOCK]
                length = len(chunk)
                chunk += bytes(count * BLOCK - length)
                self.write(start + k, chunk)
                bp = pack_bp(start + k, count, BP_DATA, self.txg, xxh64(chunk))
                self.set((obj, K_EXTENT, at), bp + struct.pack("<Q", length))
            done += got
        return nblocks

    def put_file(self, path, data, mtime_ns):
        """Creates or replaces the file at `path`."""
        found = self.lookup(path)
        if found and found[1]:
            sys.exit(f"kfs: {path} is a folder")
        if found:
            obj = found[0]
            self.drop_data(obj)
            f = self.inode(obj)
        else:
            d, name = self.parent(path)
            obj = self.new_object()
            f = [S_IFREG | 0o644, 0, 0, 1, 0] + [mtime_ns] * 4 + [0]
            self.add_entry(d, name, obj, False)
            self.touch(d)
        f[9] = self.write_data(obj, data)
        f[2] = len(data)
        f[6] = f[7] = mtime_ns
        self.put_inode(obj, f)

    def put(self, host, path):
        """Copies a host file, or a folder with everything in it, to `path`."""
        if os.path.isdir(host):
            self.mkdir(path)
            for name in sorted(os.listdir(host)):
                self.put(os.path.join(host, name), path.rstrip("/") + "/" + name)
        else:
            with open(host, "rb") as f:
                self.put_file(path, f.read(), int(os.stat(host).st_mtime * 1e9))

    def remove(self, path):
        """Deletes a file, or a folder with everything in it."""
        found = self.lookup(path)
        if not found:
            sys.exit(f"kfs: {path}: no such file or folder")
        d, name = self.parent(path)
        key = next(k for n, c, isd, k in self.entries(d) if n == name)
        self.delete(key)
        self.touch(d, -1 if found[1] else 0)
        stack = [found[0]]
        while stack:
            obj = stack.pop()
            for n, c, isd, k in self.entries(obj):
                stack.append(c)
            self.drop_data(obj)
            for k in self.of(obj):
                self.delete(k)

    def commit(self):
        if self.changed:
            items = sorted(self.items.items())
            root, depth = write_tree(items, self.alloc1, self.write, self.txg)
            # Places for the bitmap and its index first, then the frees: the
            # bits written are final, and nothing new lands on an old block.
            nbm = len(self.bits) // BLOCK
            places = iter([self.alloc1() for _ in range(nbm + index_nodes(nbm))])
            for bp in self.frees:
                set_bits(self.bits, bp["block"], min(bp["block"] + bp["count"], self.total), 0)
            bitmap_root = write_bitmap(self.bits, lambda: next(places), self.write, self.txg)
            self.f.flush()
            os.fsync(self.f.fileno())
            sb = self.raw_sb
            struct.pack_into("<Q", sb, 24, self.txg)
            sb[40:72] = root
            sb[72:104] = bitmap_root
            struct.pack_into("<QQ", sb, 136, self.free_count(), self.next_object)
            struct.pack_into("<Q", sb, BLOCK - 8, xxh64(bytes(sb[:BLOCK - 8])))
            self.write(SB_FIRST + self.txg % SB_SLOTS, bytes(sb))
            self.f.flush()
            os.fsync(self.f.fileno())
        self.f.close()


def main():
    ap = argparse.ArgumentParser(description="KonjacFS images on the host")
    sub = ap.add_subparsers(dest="cmd", required=True)
    m = sub.add_parser("mkfs")
    m.add_argument("source")
    m.add_argument("image")
    m.add_argument("--size", default="512M")
    m.add_argument("--label", default="KONJACOS")
    pu = sub.add_parser("put")
    pu.add_argument("image")
    pu.add_argument("source")
    pu.add_argument("dest", nargs="?")
    for name in ("ls", "cat", "get", "check", "mkdir", "rm"):
        p = sub.add_parser(name)
        p.add_argument("image")
        if name in ("ls",):
            p.add_argument("path", nargs="?", default="/")
        if name in ("cat", "get", "mkdir", "rm"):
            p.add_argument("path")
        if name == "get":
            p.add_argument("dest")
    a = ap.parse_args()
    if a.cmd == "mkfs":
        mkfs(a.source, a.image, parse_size(a.size), a.label)
    elif a.cmd == "check":
        check(a.image)
    elif a.cmd in ("put", "mkdir", "rm"):
        vol = Volume(a.image)
        if a.cmd == "put":
            dest = a.dest or "/" + os.path.basename(os.path.normpath(a.source))
            found = vol.lookup(dest)
            if found and found[1] and not os.path.isdir(a.source):
                dest = dest.rstrip("/") + "/" + os.path.basename(a.source)
            vol.put(a.source, dest)
        elif a.cmd == "mkdir":
            vol.mkdir(a.path)
        else:
            vol.remove(a.path)
        vol.commit()
    else:
        img = Image(a.image)
        obj = img.lookup(a.path)
        if a.cmd == "ls":
            for name, child, t in img.listing(obj):
                ino = img.inode(child)
                if t == T_DIR:
                    print(f"  <DIR>     {name}")
                else:
                    print(f"  {ino['size']:>9} {name}")
        elif a.cmd == "cat":
            sys.stdout.buffer.write(img.contents(obj))
        else:
            with open(a.dest, "wb") as f:
                f.write(img.contents(obj))


if __name__ == "__main__":
    main()
