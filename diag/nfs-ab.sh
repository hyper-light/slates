#!/bin/bash
# Diagnostics, never merged (A-192's A/B): through the product's own NFSv3 mount on macOS, one arm's daemon:
# 1. the measured case: a page dirtied through a shared map and not synced while another path replaces the file;
# 2. throughput and memory through the mount: a sequential write and read, and rounds of map stores synced.
# Usage: nfs-ab.sh ARM SLATES_BIN WORK
set -u
ARM="$1"
S="$2"
W="$3"
I=nfsab-$ARM-$$
M=$W/mnt
mkdir -p "$M"
"$S" --instance "$I" anchor --quick --shards 1 2> "$W/anchor-$I.err" &
A=$!
cleanup() {
  # Unmount first, always; a daemon stopped under a live macOS NFS mount has panicked this kernel.
  if mount | grep -q " on $M "; then
    "$S" --instance "$I" unmount "$M" || umount "$M"
  fi
  if mount | grep -q " on $M "; then
    echo "AB-KERNEL $ARM: STILL MOUNTED at $M: the daemon (pid $A) is left running"
    return
  fi
  kill "$A" 2>/dev/null
  wait "$A" 2>/dev/null
}
trap cleanup EXIT
for _ in $(seq 1 600); do "$S" --instance "$I" volume list >/dev/null 2>&1 && break; sleep 0.2; done
"$S" --instance "$I" bootstrap root >/dev/null
ID=$("$S" --instance "$I" volume create maps --bounded 512MiB | awk '/^id:/{print $2}')
ATT=$("$S" --instance "$I" attach "$ID" --write | awk '/^attachment:/{print $2}')
PAGE=$(getconf PAGESIZE)
head -c "$PAGE" /dev/zero | tr '\0' 'A' | "$S" --instance "$I" write "$ID" "$ATT" map.bin >/dev/null
"$S" --instance "$I" mount "$ID" "$M" || exit 1
DPID=$(grep -o 'daemon pid [0-9]*' "$W/anchor-$I.err" | tail -1 | awk '{print $3}')
python3 -I - "$M" "$PAGE" "$S" "$I" "$ID" "$ATT" "$ARM" "$DPID" <<'PY'
import mmap, os, subprocess, sys, time
mnt, page, s, inst, vid, att, arm, dpid = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6], sys.argv[7], sys.argv[8]
path = os.path.join(mnt, "map.bin")

# 1. The measured case.
f = open(path, "r+b")
m = mmap.mmap(f.fileno(), page, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
m[0:1] = b"Z"
subprocess.run([s, "--instance", inst, "write", vid, att, "map.bin"], input=b"C" * page, check=True, capture_output=True)
os.stat(path)
synced = "ok"
try:
    m.flush()
except OSError as e:
    synced = "refused errno %d" % e.errno
try:
    m.close()
    f.close()
except OSError as e:
    synced += ", close refused errno %d" % e.errno
out = subprocess.run([s, "--instance", inst, "read", vid, "map.bin"], capture_output=True, check=True).stdout
print("AB-KERNEL %s case: msync %s; the file: C %d, A %d, Z %d, length %d" % (arm, synced, out.count(b"C"), out.count(b"A"), out.count(b"Z"), len(out)), flush=True)

# 2. Throughput through the mount.
big = os.path.join(mnt, "big")
chunk = b"B" * (1 << 20)
started = time.perf_counter()
with open(big, "wb") as w:
    for _ in range(64):
        w.write(chunk)
    w.flush()
    os.fsync(w.fileno())
seq_write = time.perf_counter() - started
started = time.perf_counter()
with open(big, "rb") as r:
    while r.read(1 << 20):
        pass
seq_read = time.perf_counter() - started
maps = os.path.join(mnt, "maps")
size = 16 << 20
with open(maps, "wb") as w:
    w.truncate(size)
f = open(maps, "r+b")
m = mmap.mmap(f.fileno(), size, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
rounds = []
for round_ in range(10):
    started = time.perf_counter()
    for at in range(0, size, page):
        m[at:at + 1] = bytes([round_ + 1])
    m.flush()
    rounds.append(time.perf_counter() - started)
m.close()
f.close()
rss = subprocess.run(["ps", "-o", "rss=", "-p", dpid], capture_output=True).stdout.decode().strip()
status = subprocess.run([s, "--instance", inst, "status"], capture_output=True).stdout.decode()
conflicts = sum(int(l.split(":")[1]) for l in status.splitlines() if "nfs.writeback_conflicts:" in l)
rounds.sort()
print("AB-KERNEL %s perf: 64 MiB write+fsync %.3f s, read %.3f s; 16 MiB map store+msync rounds median %.3f s, best %.3f s, worst %.3f s; daemon rss %s KiB; conflicts %d"
      % (arm, seq_write, seq_read, rounds[len(rounds) // 2], rounds[0], rounds[-1], rss, conflicts), flush=True)
PY
