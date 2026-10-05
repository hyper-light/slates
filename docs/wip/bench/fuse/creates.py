import os, sys, time
d = sys.argv[1]; n = int(sys.argv[2]); os.makedirs(d, exist_ok=True)
lat = {"open": [], "write": [], "close": [], "stat": []}
t_all = time.perf_counter()
for i in range(n):
    p = os.path.join(d, f"f{i}")
    t = time.perf_counter(); fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644); lat["open"].append(time.perf_counter() - t)
    t = time.perf_counter(); os.write(fd, b"line %d\n" % i); lat["write"].append(time.perf_counter() - t)
    t = time.perf_counter(); os.close(fd); lat["close"].append(time.perf_counter() - t)
    t = time.perf_counter(); os.stat(p); lat["stat"].append(time.perf_counter() - t)
total = time.perf_counter() - t_all
q = lambda v, p: sorted(v)[min(len(v) - 1, int(p * len(v)))] * 1e6
print(f"{n} creates in {total:.2f} s; " + "; ".join(f"{k} p50 {q(v,.5):.0f} us p99 {q(v,.99):.0f} us" for k, v in lat.items()))
