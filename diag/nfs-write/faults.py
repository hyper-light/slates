# Cost of writing fresh shared-memfd pages, the shape of slates' content arena (an anchor-held memfd mapped shared):
# per-page first-touch faults; MADV_POPULATE_WRITE per 64 KiB or per 1 MiB on the writer's thread; the same populate
# run ahead of the writer on a second thread ("elsewhere"); and resident pages. Best of N, all N shown.
# Usage: python3 -I faults.py [MIB] [N]
import ctypes, mmap, os, sys, threading, time
libc = ctypes.CDLL(None, use_errno=True)
libc.madvise.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
MADV_REMOVE, MADV_POPULATE_WRITE = 9, 23
MIB = int(sys.argv[1]) if len(sys.argv) > 1 else 512
N = int(sys.argv[2]) if len(sys.argv) > 2 else 5
size = MIB << 20
fd = os.memfd_create("faultprobe")
os.ftruncate(fd, size)
m = mmap.mmap(fd, size, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
base = ctypes.addressof(ctypes.c_char.from_buffer(m))
src = os.urandom(256 << 10)
STEP = 1 << 20
def drop():
    assert libc.madvise(base, size, MADV_REMOVE) == 0, ctypes.get_errno()
def copy_range(lo, hi):
    for off in range(lo, hi, len(src)):
        ctypes.memmove(base + off, src, len(src))
def populate(lo, hi, block):
    for off in range(lo, hi, block):
        assert libc.madvise(base + off, block, MADV_POPULATE_WRITE) == 0, ctypes.get_errno()
def arm(block):
    drop(); t = time.perf_counter()
    if block: populate(0, size, block)
    copy_range(0, size)
    return MIB / (time.perf_counter() - t)
def ahead():
    drop(); done = [0]
    def helper():
        for off in range(0, size, STEP):
            populate(off, off + STEP, STEP); done[0] = off + STEP
    t = time.perf_counter(); h = threading.Thread(target=helper); h.start()
    for off in range(0, size, STEP):
        copy_range(off, off + STEP)  # never waits: a page the helper has not reached faults here, as today
    h.join()
    return MIB / (time.perf_counter() - t)
def warm():
    t = time.perf_counter(); copy_range(0, size); return MIB / (time.perf_counter() - t)
rows = {"faults": [], "populate 64K": [], "populate 1M": [], "ahead, 2nd thread": [], "resident": []}
for _ in range(N):
    rows["faults"].append(arm(0)); rows["populate 64K"].append(arm(64 << 10)); rows["populate 1M"].append(arm(1 << 20))
    rows["ahead, 2nd thread"].append(ahead()); rows["resident"].append(warm())
print("load", open("/proc/loadavg").read().split()[:3], "cpus", os.cpu_count(), os.uname().release, os.uname().machine)
for k, v in rows.items():
    print(f"{k:18s} best {max(v):8.0f} MiB/s  all " + " ".join(f"{x:.0f}" for x in v))
