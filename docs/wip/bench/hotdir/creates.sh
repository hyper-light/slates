#!/bin/bash
# A single writer's create storm on Linux's own NFSv4.2 client against a daemon in the same privileged container: for
# SECONDS (default 6), create a file, write 4 KiB and close it, timing each. Every OPEN and CLOSE is a compound whose
# reply the client asks to be kept (`sa_cachethis`), so this is the path A-127's call records add work to. Prints the
# creates per second and the p50 and p99 of one create. Run from the repository root with a Linux release build of
# slates-cli at <dir>/release/slates:
#   docker run --rm --privileged -v <dir>:/target:ro -v "$PWD/docs/wip/bench/hotdir":/bench:ro \
#     python:3.12-slim-trixie bash /bench/creates.sh
# Alternate two builds' runs to compare them; the host's load moves both.
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
slates --instance cs anchor --quick --shards 4 >/tmp/anchor.out 2>/tmp/anchor.err &
for _ in $(seq 1 200); do slates --instance cs volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance cs bootstrap root >/dev/null 2>&1
ID=$(slates --instance cs volume create cs --bounded 256MiB 2>&1 | awk '/^id/{print $2}')
slates --instance cs export "$ID" > /tmp/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /tmp/export.txt); EXPORT=$(awk '/^export:/{print $2}' /tmp/export.txt)
mkdir -p /mnt/slates
# Unmount on every exit path while the daemon still runs (a hard mount whose server is this container's own exiting
# daemon wedges the container's teardown).
trap "umount -f -l /mnt/slates 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/slates || { echo "mount failed"; exit 1; }
SECONDS_RUN=${SECONDS_RUN:-6} python3 - /mnt/slates <<'PY'
import os, sys, time
root = sys.argv[1]; d = os.path.join(root, "c"); os.makedirs(d, exist_ok=True)
payload = os.urandom(4096)
span = float(os.environ["SECONDS_RUN"])
t0 = time.time(); n = 0; lat = []
while time.time() - t0 < span:
    s = time.perf_counter_ns()
    fd = os.open(os.path.join(d, f"f{n}"), os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
    os.write(fd, payload); os.close(fd)
    lat.append(time.perf_counter_ns() - s); n += 1
lat.sort()
print(f"creates: {n} in {span:.0f} s = {n/span:.0f}/s, p50 {lat[len(lat)//2]/1e3:.1f} us, p99 {lat[int(len(lat)*.99)]/1e3:.1f} us")
PY
umount /mnt/slates
