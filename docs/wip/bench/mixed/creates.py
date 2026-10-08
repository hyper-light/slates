import os, sys, time
root, n = sys.argv[1], int(sys.argv[2])
payload = b"x" * 4096
t = []
for i in range(n):
    began = time.perf_counter_ns()
    fd = os.open(os.path.join(root, f"c{i}"), os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
    os.write(fd, payload)
    os.close(fd)
    t.append(time.perf_counter_ns() - began)
t.sort()
print(f"creates {n} p50 {t[n//2]/1000:.1f}us p99 {t[min(n-1, int(n*0.99))]/1000:.1f}us")
