#!/bin/bash
# Random small writes with frequent fsyncs (a database's pattern) through Linux's own NFSv4.2 client against a daemon
# in the same privileged container: a 512 MiB file is written once, then each round makes WRITES random 4 KiB writes
# at page-aligned offsets with an fsync every SYNC_EVERY writes; prints the round's time, fsyncs per second, each
# fsync's latency at p50, p99, p999 and max, and the daemon's CPU seconds (utime + stime from /proc). The mount is removed on every exit path while the daemon runs.
#   head -c 536870912 /dev/urandom > <dir>/payload.bin; cp random.sh <dir>/
#   docker run --rm --privileged -v <linux slates>:/slates:ro -v <dir>:/bench:ro \
#     python:3.12-slim-trixie bash /bench/random.sh
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common >/dev/null 2>&1
cp /slates /usr/local/bin/slates
slates --instance ow anchor --quick --shards 4 >/tmp/anchor.out 2>/tmp/anchor.err &
ANCHOR=$!
for _ in $(seq 1 200); do slates --instance ow volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance ow bootstrap root >/dev/null 2>&1
slates --instance ow volume create ow --bounded 2GiB > /tmp/create.txt 2>&1
ID=$(awk '/^id/{print $2}' /tmp/create.txt)
slates --instance ow export "$ID" > /tmp/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /tmp/export.txt); EXPORT=$(awk '/^export:/{print $2}' /tmp/export.txt)
mkdir -p /mnt/slates
trap "umount -f -l /mnt/slates 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/slates || { echo "mount failed"; exit 1; }
grep -o "wsize=[0-9]*" /proc/mounts | head -1
# The daemon is the anchor's child (no procps in the image: read the parent from /proc).
DAEMON=$(awk -v a="$ANCHOR" '/^PPid:/ && $2 == a {split(FILENAME, p, "/"); print p[3]; exit}' /proc/[0-9]*/status 2>/dev/null)
echo "anchor $ANCHOR daemon $DAEMON"
cpu() { awk '{print ($14 + $15) / 100}' "/proc/$1/stat"; }
if [ -n "${PROFILE:-}" ]; then apt-get install -y -qq linux-perf >/dev/null 2>&1; fi
python3 - /bench/payload.bin /mnt/slates/f <<'PY'
import os, sys
data = open(sys.argv[1], 'rb').read()
fd = os.open(sys.argv[2], os.O_CREAT | os.O_WRONLY, 0o644)
for off in range(0, len(data), 1 << 20):
    os.write(fd, data[off:off + (1 << 20)])
os.fsync(fd); os.close(fd)
PY
for round in 1 2 3; do
  before=$(cpu "$DAEMON")
  python3 - /mnt/slates/f "$round" <<'PY'
import os, random, sys, time
WRITES, SYNC_EVERY, PAGE = 4000, 8, 4096
rng = random.Random(int(sys.argv[2]))
fd = os.open(sys.argv[1], os.O_WRONLY)
pages = os.fstat(fd).st_size // PAGE
block = os.urandom(PAGE)
syncs = []
t = time.time()
for n in range(WRITES):
    os.pwrite(fd, block, rng.randrange(pages) * PAGE)
    if (n + 1) % SYNC_EVERY == 0:
        s = time.perf_counter_ns()
        os.fsync(fd)
        syncs.append(time.perf_counter_ns() - s)
os.close(fd)
d = time.time() - t
syncs.sort()
at = lambda q: syncs[min(len(syncs) - 1, int(q * len(syncs)))] / 1e6
print(f"random {WRITES} x 4 KiB, fsync every {SYNC_EVERY}: {d:.2f} s = {WRITES // SYNC_EVERY / d:.0f} fsyncs/s; "
      f"fsync p50 {at(0.5):.2f} p99 {at(0.99):.2f} p999 {at(0.999):.2f} max {syncs[-1] / 1e6:.2f} ms", end="")
PY
  echo " daemon cpu $(python3 -c "print(round($(cpu "$DAEMON") - $before, 2))") s"
done
awk '/^device .* mounted on \/mnt\/slates/{f=1} f&&/per-op statistics/{p=1;next} p&&NF==0{exit} p{print}' /proc/self/mountstats | awk '$2>0 && ($1=="WRITE:"||$1=="COMMIT:") {print $1, $2, "ops"}'
umount /mnt/slates
