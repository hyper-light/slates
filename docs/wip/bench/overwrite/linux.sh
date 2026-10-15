#!/bin/bash
# Sequential overwrite through Linux's own NFSv4.2 client against a daemon in the same privileged container: a 512 MiB
# file overwritten PASSES times in 1 MiB writes with an fsync per pass, three rounds; prints throughput and the
# daemon's CPU seconds (utime + stime from /proc). The mount is removed on every exit path while the daemon runs.
# Each round prints the daemon's pid, and the end prints the client's WRITE and COMMIT counts, the anchor's log
# (restarts, lapsed heartbeats, exits) and the kernel's OOM lines, so a daemon that dies mid-run names its cause.
# PROFILE=1 samples the daemon with perf for 2 s in round 2 and prints the top symbols; a directory mounted at /out
# keeps each run's anchor log and dmesg, named by RUN.
#   head -c 536870912 /dev/urandom > <dir>/payload.bin; cp linux.sh <dir>/
#   docker run --rm --privileged -v <linux slates>:/slates:ro -v <dir>:/bench:ro \
#     python:3.12-slim-trixie bash /bench/linux.sh
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
for round in 1 2 3; do
  DAEMON=$(awk -v a="$ANCHOR" '/^PPid:/ && $2 == a {split(FILENAME, p, "/"); print p[3]; exit}' /proc/[0-9]*/status 2>/dev/null)
  echo "round $round daemon $DAEMON"
  if [ -n "${PROFILE:-}" ] && [ "$round" = 2 ]; then (sleep 0.5; perf record -F 999 -g -p "$DAEMON" -o /tmp/perf.data -- sleep 2 >/dev/null 2>&1) & fi
  before=$(cpu "$DAEMON")
  python3 - /bench/payload.bin /mnt/slates/f <<'PY'
import os, sys, time
data = open(sys.argv[1], 'rb').read()
PASSES = 6
t = time.time()
fd = os.open(sys.argv[2], os.O_CREAT | os.O_WRONLY, 0o644)
for _ in range(PASSES):
    os.lseek(fd, 0, os.SEEK_SET)
    for off in range(0, len(data), 1 << 20):
        os.write(fd, data[off:off + (1 << 20)])
    os.fsync(fd)
os.close(fd)
d = time.time() - t
print(f"write {len(data) * PASSES >> 20} MiB in {d:.2f} s = {len(data) * PASSES / d / 2**20:.0f} MiB/s", end="")
PY
  echo " daemon cpu $(python3 -c "print(round($(cpu "$DAEMON") - $before, 2))") s"
done
awk '/^device .* mounted on \/mnt\/slates/{f=1} f&&/per-op statistics/{p=1;next} p&&NF==0{exit} p{print}' /proc/self/mountstats | awk '$2>0 && ($1=="WRITE:"||$1=="COMMIT:") {print $1, $2, "ops"}'
umount /mnt/slates
echo "== anchor.err (restarts, exits, panics)"; grep -v "slates daemon: [a-z_]* = \|initialized in" /tmp/anchor.err | tail -20
echo "== kernel OOM"; dmesg 2>/dev/null | grep -iE "out of memory|oom-kill|killed process" | tail -5
if [ -d /out ]; then cp /tmp/anchor.err "/out/anchor-${RUN:-0}.err"; dmesg > "/out/dmesg-${RUN:-0}.txt" 2>/dev/null; fi
if [ -n "${PROFILE:-}" ]; then perf report -i /tmp/perf.data --no-children --sort symbol --stdio 2>/dev/null | grep -v "^#" | grep "%" | head -40; fi
