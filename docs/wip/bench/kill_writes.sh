#!/bin/bash
# Condition 11 against A-126's written-window deltas: the daemon SIGKILLed KILLS times, GAP seconds apart, while a writer
# makes WRITES random 4 KiB writes with an fsync every SYNC_EVERY into a 64 MiB file on its NFSv4.2 volume. The writer
# keeps its own copy of the file. Each restart recovers the volume by replaying the journal, whose deltas now carry
# written windows (A-126). After the storm and the writer's last fsync, the volume is remounted (a cold client cache)
# and the file must equal the writer's copy byte for byte. Run privileged with a Linux release build:
#   docker run --rm --privileged -v <linux slates>:/slates:ro -v "$PWD/docs/wip/bench":/b:ro \
#     [-e KILLS=10 -e GAP=1.5 -e WRITES=20000] python:3.12-slim-trixie bash /b/kill_writes.sh
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common >/dev/null 2>&1
cp /slates /usr/local/bin/slates
slates --instance kw anchor --quick --shards 4 >/tmp/anchor.out 2>/tmp/anchor.err &
ANCHOR=$!
for _ in $(seq 1 300); do slates --instance kw volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance kw bootstrap root >/dev/null 2>&1
ID=$(slates --instance kw volume create kw --dynamic 1GiB 2>&1 | awk '/^id/{print $2}')
slates --instance kw export "$ID" > /tmp/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /tmp/export.txt); EXPORT=$(awk '/^export:/{print $2}' /tmp/export.txt)
OPTIONS="vers=4.2,proto=tcp,port=$PORT,hard,timeo=600"
mkdir -p /mnt/v; trap "umount -f -l /mnt/v 2>/dev/null" EXIT
mount -t nfs -o "$OPTIONS" "127.0.0.1:$EXPORT" /mnt/v || { echo MOUNT-FAILED; exit 1; }
# The daemon is the anchor's child (no procps in the image: read the parent from /proc).
daemon_pid() { awk -v a="$ANCHOR" '/^PPid:/ && $2 == a {split(FILENAME, p, "/"); print p[3]; exit}' /proc/[0-9]*/status 2>/dev/null; }
python3 - /mnt/v/f /tmp/model <<'PY' &
import os, random, sys, time
SIZE, PAGE = 64 << 20, 4096
WRITES, SYNC_EVERY = int(os.environ.get("WRITES", "20000")), int(os.environ.get("SYNC_EVERY", "8"))
rng = random.Random(9)
model = bytearray(os.urandom(SIZE))
fd = os.open(sys.argv[1], os.O_CREAT | os.O_RDWR, 0o644)
for off in range(0, SIZE, 1 << 20):
    os.pwrite(fd, model[off:off + (1 << 20)], off)
os.fsync(fd)
longest = 0
started = time.time()
for n in range(WRITES):
    at = rng.randrange(SIZE // PAGE) * PAGE
    block = rng.randbytes(PAGE)
    os.pwrite(fd, block, at)
    model[at:at + PAGE] = block
    if (n + 1) % SYNC_EVERY == 0:
        s = time.time()
        os.fsync(fd)
        longest = max(longest, time.time() - s)
os.fsync(fd)
os.close(fd)
open(sys.argv[2], "wb").write(model)
print(f"writer: {WRITES} writes, {WRITES // SYNC_EVERY} fsyncs in {time.time() - started:.1f} s; longest fsync {longest:.2f} s")
PY
WRITER=$!
sleep 2
for n in $(seq 1 "${KILLS:-10}"); do
  P=$(daemon_pid); [ -n "$P" ] && kill -9 "$P" && echo "kill $n: daemon $P"
  sleep "${GAP:-1.5}"
done
wait $WRITER || echo "WRITER-FAILED"
# A cold client cache: every byte compared below comes from the daemon that recovered last.
umount /mnt/v && mount -t nfs -o "$OPTIONS" "127.0.0.1:$EXPORT" /mnt/v || { echo REMOUNT-FAILED; exit 1; }
python3 - /mnt/v/f /tmp/model <<'PY'
import sys
got, want = open(sys.argv[1], "rb").read(), open(sys.argv[2], "rb").read()
if got == want:
    print(f"after the storm: the file reads back identical ({len(got)} bytes)")
else:
    first = next((i for i, (a, b) in enumerate(zip(got, want)) if a != b), min(len(got), len(want)))
    print(f"after the storm: the file reads back DIFFERENT (got {len(got)}, want {len(want)} bytes; first difference at {first})")
PY
echo "restarts: $(grep -c 'restarted' /tmp/anchor.err)"
grep -E "restarted|crash loop|lapsed|panicked|error" /tmp/anchor.err | tail -5
