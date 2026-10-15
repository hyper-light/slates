#!/usr/bin/env python3
"""Mixed read-write tail latency in one shared directory (condition 12's owed matrix).

Usage: mixed.py ROOT WORKERS OPS_PER_WORKER LARGE_MIB

Each worker runs OPS_PER_WORKER operations drawn with fixed weights from: create (open O_CREAT + write 4 KiB + close),
overwrite (open + write 4 KiB at 0 + close), append (open O_APPEND + write 4 KiB + close), read (open + read whole +
close), stat, rename, unlink. Every operation is timed. Then worker 0 writes one LARGE_MIB file sequentially in 1 MiB
writes, fsyncs, and reads it back, checking every byte. Prints per-operation counts and p50/p99/p999 in microseconds,
and the large file's write and read throughput. Exits non-zero on any byte mismatch.
"""
import hashlib
import os
import random
import sys
import threading
import time

ROOT, WORKERS, OPS, LARGE_MIB = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
BLOCK = 4096
WEIGHTS = [("create", 30), ("overwrite", 15), ("append", 10), ("read", 25), ("stat", 10), ("rename", 5), ("unlink", 5)]
timings = {name: [] for name, _ in WEIGHTS}
lock = threading.Lock()
mismatches = []


def timed(name, action):
    began = time.perf_counter_ns()
    action()
    elapsed = time.perf_counter_ns() - began
    with lock:
        timings[name].append(elapsed)


def worker(index):
    rng = random.Random(index)
    mine = []
    payload = bytes(rng.getrandbits(8) for _ in range(BLOCK))
    choices = [name for name, weight in WEIGHTS for _ in range(weight)]
    serial = 0
    for _ in range(OPS):
        op = rng.choice(choices)
        if op in ("overwrite", "append", "read", "stat", "rename", "unlink") and not mine:
            op = "create"
        if op == "create":
            path = os.path.join(ROOT, f"w{index}-{serial}")
            serial += 1

            def create():
                fd = os.open(path, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
                os.write(fd, payload)
                os.close(fd)

            timed(op, create)
            mine.append(path)
        elif op == "overwrite":
            path = rng.choice(mine)

            def overwrite():
                fd = os.open(path, os.O_WRONLY)
                os.pwrite(fd, payload, 0)
                os.close(fd)

            timed(op, overwrite)
        elif op == "append":
            path = rng.choice(mine)

            def append():
                fd = os.open(path, os.O_WRONLY | os.O_APPEND)
                os.write(fd, payload)
                os.close(fd)

            timed(op, append)
        elif op == "read":
            path = rng.choice(mine)

            def read():
                with open(path, "rb") as handle:
                    data = handle.read()
                if data[:BLOCK] != payload:
                    mismatches.append(path)

            timed(op, read)
        elif op == "stat":
            path = rng.choice(mine)
            timed(op, lambda: os.stat(path))
        elif op == "rename":
            position = rng.randrange(len(mine))
            old = mine[position]
            new = os.path.join(ROOT, f"w{index}-{serial}")
            serial += 1
            timed(op, lambda: os.rename(old, new))
            mine[position] = new
        elif op == "unlink":
            path = mine.pop(rng.randrange(len(mine)))
            timed(op, lambda: os.unlink(path))


def quantile(values, q):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))]


threads = [threading.Thread(target=worker, args=(index,)) for index in range(WORKERS)]
began = time.perf_counter()
for thread in threads:
    thread.start()
for thread in threads:
    thread.join()
wall = time.perf_counter() - began
print(f"workers {WORKERS} ops {WORKERS * OPS} wall {wall:.2f}s")
for name, values in timings.items():
    if values:
        print(
            f"{name:9s} n={len(values):6d} p50={quantile(values, 0.5) / 1000:9.1f}us "
            f"p99={quantile(values, 0.99) / 1000:9.1f}us p999={quantile(values, 0.999) / 1000:9.1f}us"
        )

large = os.path.join(ROOT, "large.bin")
rng = random.Random(7)
chunk = bytes(rng.getrandbits(8) for _ in range(1 << 20))
digest = hashlib.sha256()
began = time.perf_counter()
fd = os.open(large, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
for _ in range(LARGE_MIB):
    os.write(fd, chunk)
    digest.update(chunk)
os.fsync(fd)
os.close(fd)
write_s = time.perf_counter() - began
began = time.perf_counter()
back = hashlib.sha256()
with open(large, "rb") as handle:
    while True:
        data = handle.read(1 << 20)
        if not data:
            break
        back.update(data)
read_s = time.perf_counter() - began
same = back.digest() == digest.digest()
print(f"large {LARGE_MIB} MiB write {LARGE_MIB / write_s:.1f} MiB/s read {LARGE_MIB / read_s:.1f} MiB/s identical={same}")
if mismatches or not same:
    print(f"MISMATCH: {len(mismatches)} small reads, large identical={same}")
    sys.exit(1)
