#!/usr/bin/env python3
"""Times `open(O_CREAT) + write + fsync + close` as one span, N times, reporting p50/p99 in microseconds —
the `create+fsync` arm of the condition-12 comparison. Usage: fsync.py DIR N."""
import os, sys, time

d = sys.argv[1]
n = int(sys.argv[2])
os.makedirs(d, exist_ok=True)
lat = []
t_all = time.perf_counter()
for i in range(n):
    p = os.path.join(d, f"s{i}")
    t = time.perf_counter()
    fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    os.write(fd, b"line %d\n" % i)
    os.fsync(fd)
    os.close(fd)
    lat.append(time.perf_counter() - t)
total = time.perf_counter() - t_all
q = lambda v, pq: sorted(v)[min(len(v) - 1, int(pq * len(v)))] * 1e6
print(f"{n} create+fsync in {total:.2f} s; p50 {q(lat, .5):.0f} us p99 {q(lat, .99):.0f} us")
