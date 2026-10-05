# A hot-directory storm (Tectonic §6.3's non-ideal pattern): WORKERS threads in one shared directory, each timing
# every operation. Phases: create+write 4 KiB+close, then open+read+close at random, stat at random, and readdir.
import os, random, sys, threading, time
root, workers, per_worker = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
hot = os.path.join(root, "hot")
os.makedirs(hot, exist_ok=True)
payload = os.urandom(4096)
lat = {"create_write": [], "open_read": [], "stat": [], "readdir": []}
lock = threading.Lock()
barrier = threading.Barrier(workers)

def timed(kind, fn):
    t0 = time.perf_counter_ns()
    fn()
    dt = time.perf_counter_ns() - t0
    return kind, dt

def worker(w):
    mine = []
    barrier.wait()
    for i in range(per_worker):
        name = os.path.join(hot, f"w{w:03d}-{i:05d}")
        def cw():
            fd = os.open(name, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
            os.write(fd, payload)
            os.close(fd)
        mine.append(timed("create_write", cw))
    barrier.wait()
    rng = random.Random(w)
    for i in range(per_worker):
        name = os.path.join(hot, f"w{rng.randrange(workers):03d}-{rng.randrange(per_worker):05d}")
        def orr():
            fd = os.open(name, os.O_RDONLY)
            os.read(fd, 4096)
            os.close(fd)
        mine.append(timed("open_read", orr))
        mine.append(timed("stat", lambda: os.stat(name)))
        if i % 100 == 0:
            mine.append(timed("readdir", lambda: os.listdir(hot)))
    with lock:
        for kind, dt in mine:
            lat[kind].append(dt)

t0 = time.time()
threads = [threading.Thread(target=worker, args=(w,)) for w in range(workers)]
for t in threads: t.start()
for t in threads: t.join()
elapsed = time.time() - t0
def q(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))]
total = sum(len(v) for v in lat.values())
print(f"workers={workers} ops={total} elapsed={elapsed:.2f}s ops_per_s={total/elapsed:.0f}")
for kind, xs in lat.items():
    if xs:
        print(f"{kind:13s} n={len(xs):6d} p50={q(xs,.50)/1e3:9.1f}us p99={q(xs,.99)/1e3:9.1f}us p999={q(xs,.999)/1e3:9.1f}us max={max(xs)/1e3:9.1f}us")
