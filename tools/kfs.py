#!/usr/bin/env python3
"""KonjacFS (KFS) on the host: build an image, look inside one, check one.

    kfs.py mkfs SOURCE_DIR IMAGE [--size 512M] [--label NAME]
    kfs.py ls IMAGE [PATH]
    kfs.py cat IMAGE PATH
    kfs.py get IMAGE PATH DEST
    kfs.py check IMAGE

The format is described in docs/kfs-design.md. This file is written from
that document, separately from the kernel's `kfs.rs`, so the two check
each other: an image this builds has to read back correctly in KonjacOS,
and `check` verifies every checksum, the tree's ordering and the
free-space bitmap. Standard library only.
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


def mkfs(src, image, size, label):
    total = size // BLOCK
    if total < 1024:
        sys.exit("kfs: the volume must be at least 4 MiB")
    b = Builder(total)
    txg = 1

    # The bitmap's blocks and index come first, so every block in use ends
    # up in one run from 0 to wherever allocation stops.
    nbitmap = -(-total // BITS_PER_BITMAP)
    index_levels = []  # node counts per level, bottom up
    n = nbitmap
    while True:
        n = -(-n // INDEX_MAX)
        index_levels.append(n)
        if n == 1:
            break
    bitmap_first = b.alloc(nbitmap)
    index_first = b.alloc(sum(index_levels))

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

    # Leaves: as many items as fit, data packed from the back.
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
        blk = b.alloc(1)
        out = bytearray(BLOCK)
        out[0:HEADER] = node_header(0, TREE_FS, len(its), txg)
        end = BLOCK
        for i, ((obj, kind, off), data) in enumerate(its):
            end -= len(data)
            out[end:end + len(data)] = data
            h = HEADER + i * ITEM_HEADER
            out[h:h + ITEM_HEADER] = struct.pack("<QQBBHHH", obj, off, kind, 0, 0, end, len(data))
        out = bytes(out)
        b.put(blk, out)
        return its[0][0], pack_bp(blk, 1, BP_NODE, txg, xxh64(out))

    level = [write_leaf(l) for l in leaves]
    depth = 0
    while len(level) > 1:
        depth += 1
        up = []
        for i in range(0, len(level), INTERIOR_MAX):
            group = level[i:i + INTERIOR_MAX]
            blk = b.alloc(1)
            out = bytearray(BLOCK)
            out[0:HEADER] = node_header(depth, TREE_FS, len(group), txg)
            for j, (key, bp) in enumerate(group):
                o = HEADER + j * INTERIOR_ENTRY
                out[o:o + KEY] = pack_key(key[0], key[1], key[2])
                out[o + KEY:o + INTERIOR_ENTRY] = bp
            out = bytes(out)
            b.put(blk, out)
            up.append((group[0][0], pack_bp(blk, 1, BP_NODE, txg, xxh64(out))))
        level = up
    fs_root = level[0][1]

    # Bitmap: every block from 0 up to the allocation point is in use.
    used_blocks = b.next
    bitmap_bps = []
    for i in range(nbitmap):
        out = bytearray(BLOCK)
        lo = i * BITS_PER_BITMAP
        hi = min(lo + BITS_PER_BITMAP, used_blocks)
        for blkno in range(lo, hi):
            out[(blkno - lo) >> 3] |= 1 << ((blkno - lo) & 7)
        # Bits past the end of the volume are set too, so nothing allocates them.
        for blkno in range(max(lo, total), lo + BITS_PER_BITMAP):
            out[(blkno - lo) >> 3] |= 1 << ((blkno - lo) & 7)
        out = bytes(out)
        b.put(bitmap_first + i, out)
        bitmap_bps.append(pack_bp(bitmap_first + i, 1, BP_BITMAP, txg, xxh64(out)))
    level = bitmap_bps
    blk = index_first
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
            b.put(blk, out)
            up.append(pack_bp(blk, 1, BP_BITMAP_INDEX, txg, xxh64(out)))
            blk += 1
        level = up
        if len(level) == 1:
            break
    bitmap_root = level[0]

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
    b.put(SB_FIRST + txg % SB_SLOTS, bytes(sb))

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


def check(path):
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

    print(f"kfs: {path}: txg {sb['txg']} (slot {sb['slot']}), {sb['total']} blocks, {sb['free']} free, "
          f"{len(inodes)} objects, label {sb['label']!r}")
    for p in problems[:50]:
        print("  PROBLEM:", p)
    if problems:
        sys.exit(1)
    print("kfs: no problems found")


def main():
    ap = argparse.ArgumentParser(description="KonjacFS images on the host")
    sub = ap.add_subparsers(dest="cmd", required=True)
    m = sub.add_parser("mkfs")
    m.add_argument("source")
    m.add_argument("image")
    m.add_argument("--size", default="512M")
    m.add_argument("--label", default="KONJACOS")
    for name in ("ls", "cat", "get", "check"):
        p = sub.add_parser(name)
        p.add_argument("image")
        if name in ("ls",):
            p.add_argument("path", nargs="?", default="/")
        if name in ("cat", "get"):
            p.add_argument("path")
        if name == "get":
            p.add_argument("dest")
    a = ap.parse_args()
    if a.cmd == "mkfs":
        mkfs(a.source, a.image, parse_size(a.size), a.label)
    elif a.cmd == "check":
        check(a.image)
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
