import os, sys, time
root, files, rounds = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
prefix = sys.argv[4] if len(sys.argv) > 4 else "g"
d = os.path.join(root, "ls"); os.makedirs(d, exist_ok=True)
if files:
    for i in range(files):
        open(os.path.join(d, f"f{i:06d}"), "w").close()
    sys.exit(0)
times = []
for r in range(rounds):
    open(os.path.join(d, f"{prefix}{r:06d}"), "w").close()
    t = time.perf_counter(); n = len(os.listdir(d)); times.append(time.perf_counter() - t)
times.sort()
q = lambda p: times[min(len(times) - 1, int(p * len(times)))] * 1e3
print(f"listings={rounds} entries~{n} p50={q(0.5):.2f}ms p99={q(0.99):.2f}ms max={times[-1]*1e3:.2f}ms")
