#!/usr/bin/env python3
"""Bit-rot test for KonjacFS: flips one random byte in a random block of a
fresh image, checks the host checker notices, then boots KonjacOS on it
and runs `verify`, which reads every file.

    tools/bitrot_test.py [RUNS] [--seed N]

What must happen depends on what was hit:
  - a superblock slot: nothing is lost (another slot has the same state)
    and every file reads;
  - a file's data: that file fails with a checksum error, every other
    file reads;
  - a tree node: some files fail, the rest read, nothing crashes;
  - the root node or the free-space bitmap: the volume either refuses
    to mount, saying it's damaged, or mounts the previous commit (the
    base image has two), which must then read back whole.
Never wrong data, a crash, or damage the host checker misses. Needs
image.iso (`make iso`).
"""

import argparse
import os
import random
import re
import shutil
import struct
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import kfs  # noqa: E402
from crash_test import ROOT, SETTLE, Qemu  # noqa: E402

VERIFY_TIMEOUT = 60


def build_base(path):
    """A small image with the usual files plus 300 more, so the tree has
    many leaves and both inline and extent data."""
    kfs.mkfs(os.path.join(ROOT, "disk_root"), path, kfs.parse_size("32M"), "BITROT")
    vol = kfs.Volume(path)
    rng = random.Random(7)
    for i in range(300):
        size = rng.choice([10, 500, 2048, 3000, 9000, 70000])
        vol.put_file(f"/many/d{i % 7}/f{i}", bytes(rng.randrange(256) for _ in range(size)), time.time_ns())
    vol.commit()


def blocks_of(path):
    """`{category: [(block, file path or None), ...]}` for every block in use."""
    img = kfs.Image(path)
    cats = {"superblock": [(kfs.SB_FIRST + s, None) for s in range(kfs.SB_SLOTS)],
            "root node": [], "tree node": [], "file data": [], "bitmap": []}
    items = {}

    def walk(bp, root):
        cats["root node" if root else "tree node"].append((bp["block"], None))
        b, level, tree, count = img.node(bp)
        if level == 0:
            for i in range(count):
                obj, off, kind, _p, _q, doff, dlen = struct.unpack_from("<QQBBHHH", b, kfs.HEADER + i * kfs.ITEM_HEADER)
                items.setdefault(obj, []).append(((obj, kind, off), b[doff:doff + dlen]))
        else:
            for i in range(count):
                walk(kfs.unpack_bp(b, kfs.HEADER + i * kfs.INTERIOR_ENTRY + kfs.KEY), False)
    walk(img.sb["fs_root"], True)

    def walk_index(bp):
        cats["bitmap"].append((bp["block"], None))
        b, level, tree, count = img.node(bp)
        for i in range(count):
            child = kfs.unpack_bp(b, kfs.HEADER + i * kfs.BP)
            if level == 1:
                cats["bitmap"].append((child["block"], None))
            else:
                walk_index(child)
    walk_index(img.sb["bitmap_root"])

    # File paths, for the data blocks.
    stack = [("", kfs.ROOT_OBJECT)]
    while stack:
        where, obj = stack.pop()
        for (o, kind, off), d in items.get(obj, []):
            if kind == kfs.K_DIR_ENTRY:
                child, t, n = struct.unpack_from("<QBB", d)
                p = where + "/" + d[10:10 + n].decode()
                if t == kfs.T_DIR:
                    stack.append((p, child))
                else:
                    for (o2, k2, off2), d2 in items.get(child, []):
                        if k2 == kfs.K_EXTENT:
                            bp = kfs.unpack_bp(d2)
                            for blk in range(bp["block"], bp["block"] + bp["count"]):
                                cats["file data"].append((blk, p))
    return cats


def main():
    ap = argparse.ArgumentParser(description="bit-rot test KonjacFS in QEMU")
    ap.add_argument("runs", nargs="?", type=int, default=20)
    ap.add_argument("--seed", type=int, default=None)
    a = ap.parse_args()
    rng = random.Random(a.seed)
    if not os.path.exists(os.path.join(ROOT, "image.iso")):
        sys.exit("bitrot_test: build image.iso first (make iso)")
    workdir = tempfile.mkdtemp(prefix="kfs-bitrot-")
    iso = shutil.copy(os.path.join(ROOT, "image.iso"), workdir)
    base = os.path.join(workdir, "base.img")
    image = os.path.join(workdir, "rot.img")
    build_base(base)
    cats = blocks_of(base)
    print("bitrot_test: blocks in use: " + ", ".join(f"{len(v)} {k}" for k, v in cats.items()))
    failures = 0
    started = time.time()
    try:
        for run in range(1, a.runs + 1):
            cat = rng.choice(list(cats))
            block, owner = rng.choice(cats[cat])
            shutil.copy(base, image)
            with open(image, "r+b") as f:
                off = block * kfs.BLOCK + rng.randrange(kfs.BLOCK)
                f.seek(off)
                byte = f.read(1)[0]
                f.seek(off)
                f.write(bytes([byte ^ rng.randrange(1, 256)]))
            what = f"{cat} (block {block}{', ' + owner if owner else ''})"
            problems = []

            # The host checker must notice anything but a superblock copy.
            try:
                _, found, _ = kfs.check_image(image)
            except (SystemExit, IOError) as e:
                found = [str(e)]
            if cat != "superblock" and not found:
                problems.append("kfs.py check didn't notice")

            q = Qemu(image, workdir, iso)
            mounted, why = q.wait_for_mount()
            summary = None
            log = ""
            if mounted:
                time.sleep(SETTLE)
                q.type("verify")
                end = time.time() + VERIFY_TIMEOUT
                while time.time() < end and summary is None:
                    time.sleep(0.3)
                    log = q.serial()
                    summary = re.search(r"verify: (\d+) files, \d+ KiB read, (\d+) errors", log)
            q.kill()
            errors = [ln for ln in log.splitlines() if ln.startswith("verify: /")]
            if "panic" in q.serial().lower():
                problems.append("the kernel panicked")
            if cat in ("superblock",):
                if not mounted or not summary or summary.group(2) != "0":
                    problems.append(f"expected everything to read: {why or (summary and summary.group(0))}")
            elif cat == "file data":
                if not summary:
                    problems.append(f"verify didn't finish: {why}")
                elif summary.group(2) != "1" or not any(ln.startswith(f"verify: {owner}:") for ln in errors):
                    problems.append(f"expected one error, for {owner}: {summary.group(0)}; {errors[:3]}")
            elif cat == "tree node":
                if not summary:
                    problems.append(f"verify didn't finish: {why}")
                elif summary.group(2) == "0":
                    problems.append("verify found nothing wrong")
            else:  # root node, bitmap
                # Either it won't mount, saying why, or it falls back to the
                # previous commit -- which must then read back whole.
                if mounted:
                    fell_back = "is damaged; using txg" in log
                    if not fell_back:
                        problems.append("the volume mounted without noticing")
                    elif not summary or summary.group(2) != "0":
                        problems.append(f"fell back, but the older state doesn't read: {summary and summary.group(0)}")
                    else:
                        why = "fell back to the previous commit; " + summary.group(0)
                elif "damaged" not in why:
                    problems.append(f"didn't say it's damaged: {why}")
            if problems:
                failures += 1
                print(f"run {run}: FAILED -- {what}: " + "; ".join(problems))
                shutil.copy(image, os.path.join(tempfile.gettempdir(), f"kfs-bitrot-fail-{run}.img"))
            else:
                result = why if (why or not summary) else summary.group(0)
                print(f"run {run}: ok -- {what}: {result.replace('KonjacFS: ', '')}")
            sys.stdout.flush()
    finally:
        shutil.rmtree(workdir, ignore_errors=True)
    print(f"bitrot_test: {a.runs} runs, {failures} failed, {(time.time() - started) / 60:.1f} min")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
