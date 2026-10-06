#!/usr/bin/env python3
"""Crash-tests KonjacFS: boots KonjacOS in QEMU with a small KFS disk as
`/`, starts `kfstest` (random writes, overwrites, renames and deletes),
kills QEMU at a random moment -- no shutdown, no flush -- and checks the
disk on the host. Then does it again on the same disk.

    tools/crash_test.py [RUNS] [--size 32M] [--image PATH] [--seed N]

Every run must leave a disk that
  - `kfs.py check` passes (checksums, tree, bitmap, link counts, sizes),
  - the next boot mounts, and
  - in which every file kfstest wrote is whole: kfstest starts each file
    of 16 bytes or more with "KFST", a tag and the size, and the rest
    follows from the tag, so a file holding a mix of old and new
    contents, or garbage, is caught.
A failing image is kept as IMAGE.fail-RUN. Needs image.iso (`make iso`).
"""

import argparse
import os
import random
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import kfs  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT_TIMEOUT = 60
# Once KFS has mounted, how long until the Terminal takes keys.
SETTLE = 4.0
# kfstest is killed this long after its command line is entered.
KILL_MIN, KILL_MAX = 0.1, 4.0

MAX_SIZE = 300_000
_BASE = bytes(((i * 31 + i // 4096) & 255) for i in range(MAX_SIZE))
_SHIFT = [bytes(((x + k) & 255) for x in range(256)) for k in range(256)]


def expected(size, tag):
    """What kfstest writes: see `content` in cmd_kfstest (commands.rs)."""
    d = bytearray(_BASE[:size].translate(_SHIFT[(tag * 7) & 255]))
    if size >= 16:
        d[0:16] = b"KFST" + struct.pack("<IQ", tag, size)
    return bytes(d)


def verify_files(path):
    """Problems with the files under /kfstest (none if it isn't there)."""
    img = kfs.Image(path)
    by_obj = {}
    for k, d in img.items():
        by_obj.setdefault(k[0], []).append((k, d))

    def children(obj):
        for (o, kind, off), d in by_obj.get(obj, []):
            if kind == kfs.K_DIR_ENTRY:
                child, t, n = struct.unpack_from("<QBB", d)
                yield d[10:10 + n].decode(errors="replace"), child, t

    def contents(obj):
        size = None
        out = None
        for (o, kind, off), d in by_obj.get(obj, []):
            if kind == kfs.K_INODE:
                size = kfs.unpack_inode(d)["size"]
                out = bytearray(size)
        for (o, kind, off), d in by_obj.get(obj, []):
            if kind == kfs.K_INLINE:
                out[0:len(d)] = d
            elif kind == kfs.K_EXTENT:
                bp = kfs.unpack_bp(d)
                (length,) = struct.unpack_from("<Q", d, kfs.BP)
                out[off:off + length] = img.read_bp(bp)[:length]
        return bytes(out)

    top = [c for c in children(kfs.ROOT_OBJECT) if c[0] == "kfstest"]
    problems = []
    checked = 0
    stack = [("/kfstest", top[0][1])] if top else []
    while stack:
        where, obj = stack.pop()
        for name, child, t in children(obj):
            p = where + "/" + name
            if t == kfs.T_DIR:
                stack.append((p, child))
                continue
            try:
                data = contents(child)
            except IOError as e:
                problems.append(f"{p}: {e}")
                continue
            # Shorter files carry no header to check them by.
            checked += 1
            if len(data) >= 16:
                magic, tag, size = data[:4], *struct.unpack_from("<IQ", data, 4)
                if magic != b"KFST" or size != len(data) or data != expected(size, tag):
                    problems.append(f"{p}: {len(data)} bytes that aren't one whole kfstest file")
    return checked, problems


class Qemu:
    def __init__(self, image, workdir, iso):
        self.log = os.path.join(workdir, "serial.log")
        self.sock = os.path.join(workdir, "monitor.sock")
        for f in (self.log, self.sock):
            if os.path.exists(f):
                os.remove(f)
        self.proc = subprocess.Popen(
            ["qemu-system-x86_64", "-m", "256M", "-no-reboot", "-boot", "order=d",
             "-cdrom", iso,
             "-drive", f"file={image},format=raw,if=virtio",
             "-display", "none", "-serial", f"file:{self.log}",
             "-monitor", f"unix:{self.sock},server,nowait"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.mon = None
        for _ in range(100):
            if os.path.exists(self.sock):
                self.mon = socket.socket(socket.AF_UNIX)
                self.mon.connect(self.sock)
                break
            time.sleep(0.05)
        if self.mon is None:
            raise RuntimeError("QEMU's monitor never appeared")

    def serial(self):
        try:
            with open(self.log, errors="replace") as f:
                return f.read()
        except FileNotFoundError:
            return ""

    def wait_for_mount(self):
        """True once KFS mounted; False if it said why it couldn't."""
        end = time.time() + BOOT_TIMEOUT
        while time.time() < end:
            log = self.serial()
            if "KonjacFS volume" in log:
                return True, ""
            if "KonjacFS:" in log:
                line = log[log.index("KonjacFS:"):].splitlines()[0]
                return False, line
            time.sleep(0.1)
        return False, "no KonjacFS line on the serial port within %ds" % BOOT_TIMEOUT

    def type(self, text):
        keys = {" ": "spc", "/": "slash", ".": "dot", "-": "minus"}
        for ch in text:
            self.mon.sendall(f"sendkey {keys.get(ch, ch)}\n".encode())
            time.sleep(0.04)
        self.mon.sendall(b"sendkey ret\n")

    def kill(self):
        self.proc.kill()
        self.proc.wait()
        self.mon.close()


def check(image):
    """`(txg, problems, files checked)`."""
    try:
        sb, problems, _ = kfs.check_image(image)
    except SystemExit as e:
        return None, [str(e)], 0
    except IOError as e:
        return None, [str(e)], 0
    if problems:
        return sb["txg"], problems, 0
    checked, more = verify_files(image)
    return sb["txg"], more, checked


def main():
    ap = argparse.ArgumentParser(description="crash-test KonjacFS in QEMU")
    ap.add_argument("runs", nargs="?", type=int, default=20)
    ap.add_argument("--size", default="32M")
    ap.add_argument("--image", default=os.path.join(tempfile.gettempdir(), "kfs-crash.img"))
    ap.add_argument("--seed", type=int, default=None)
    a = ap.parse_args()
    rng = random.Random(a.seed)
    if not os.path.exists(os.path.join(ROOT, "image.iso")):
        sys.exit("crash_test: build image.iso first (make iso)")

    # A fresh disk with the usual files on it.
    kfs.mkfs(os.path.join(ROOT, "disk_root"), a.image, kfs.parse_size(a.size), "CRASHTEST")
    workdir = tempfile.mkdtemp(prefix="kfs-crash-")
    # Its own copy, so rebuilding meanwhile doesn't change what it tests.
    iso = shutil.copy(os.path.join(ROOT, "image.iso"), workdir)
    failures = 0
    last_txg = check(a.image)[0]
    started = time.time()
    try:
        for run in range(1, a.runs + 1):
            q = Qemu(a.image, workdir, iso)
            mounted, why = q.wait_for_mount()
            if not mounted:
                q.kill()
                print(f"run {run}: FAILED -- the disk didn't mount: {why}")
                shutil.copy(a.image, f"{a.image}.fail-{run}")
                failures += 1
                break
            time.sleep(SETTLE)
            q.type(f"kfstest 1000000 {rng.randrange(1, 1 << 31)} keep")
            delay = rng.uniform(KILL_MIN, KILL_MAX)
            time.sleep(delay)
            q.kill()
            txg, problems, checked = check(a.image)
            commits = (txg or 0) - (last_txg or 0)
            if problems:
                failures += 1
                print(f"run {run}: FAILED after {commits} commits (killed {delay:.2f}s in):")
                for p in problems[:20]:
                    print("   ", p)
                shutil.copy(a.image, f"{a.image}.fail-{run}")
            else:
                print(f"run {run}: ok -- killed {delay:.2f}s in, {commits} commits since the last run, "
                      f"{checked} files whole")
            sys.stdout.flush()
            last_txg = txg if txg is not None else last_txg
        else:
            # The last crash's disk has to mount too.
            q = Qemu(a.image, workdir, iso)
            mounted, why = q.wait_for_mount()
            q.kill()
            if not mounted:
                failures += 1
                print(f"final boot: FAILED -- the disk didn't mount: {why}")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)
    mins = (time.time() - started) / 60
    print(f"crash_test: {a.runs} runs, {failures} failed, {mins:.1f} min")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
