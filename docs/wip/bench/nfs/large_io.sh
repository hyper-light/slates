#!/bin/bash
# Condition 12: large sequential I/O through one Linux NFSv4.2 client: a 512 MiB file written with `conv=fsync` from a
# random source, the client's caches dropped, then read back, ROUNDS times, with the daemon's CPU for each transfer. Run in a privileged container with the
# Linux release build at /target/release/slates:
#   docker run --rm --privileged -v <target-volume>:/target:ro -v "$PWD/docs/wip/bench/nfs":/n:ro rust:1.98.0 \
#     bash /n/large_io.sh [ROUNDS]     (SLATES names the binary when it is not /target/release/slates)
set -u
ROUNDS=${1:-5}
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common >/dev/null 2>&1
cp "${SLATES:-/target/release/slates}" /usr/local/bin/slates
slates --instance lio anchor --quick --shards 4 >/dev/null 2>&1 &
for _ in $(seq 1 300); do slates --instance lio volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance lio bootstrap root >/dev/null 2>&1
ID=$(slates --instance lio volume create lio --dynamic 4GiB 2>&1 | awk '/^id/{print $2}')
slates --instance lio export "$ID" > /tmp/e.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /tmp/e.txt); EXPORT=$(awk '/^export:/{print $2}' /tmp/e.txt)
mkdir -p /mnt/v; trap "umount -f -l /mnt/v 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/v || { echo MOUNT-FAILED; exit 1; }
dd if=/dev/urandom of=/tmp/src bs=1M count=512 status=none
dd if=/tmp/src of=/mnt/v/warm bs=1M conv=fsync status=none; rm -f /mnt/v/warm
# The daemon's own CPU (user + system, in clock ticks of /proc/PID/stat) around each transfer: what the data path costs,
# which a shared machine's scheduling moves far less than it moves throughput.
D=$(pgrep -f "slates --instance lio daemon" | head -1)
cpu_ms() { awk -v hz="$(getconf CLK_TCK)" '{print int(($14 + $15) * 1000 / hz)}' /proc/$D/stat; }
for round in $(seq 1 "$ROUNDS"); do
  C0=$(cpu_ms)
  W=$(dd if=/tmp/src of=/mnt/v/f bs=1M conv=fsync 2>&1 | awk '/copied/{print $(NF-1), $NF}')
  C1=$(cpu_ms)
  # A cold read: the mount is made again, so the client holds nothing of the file. Never the global `drop_caches`,
  # which walks every superblock in the VM and hung behind a wedged one on 2026-10-08.
  sync -f /mnt/v && umount /mnt/v && mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/v
  C2=$(cpu_ms)
  R=$(dd if=/mnt/v/f of=/dev/null bs=1M 2>&1 | awk '/copied/{print $(NF-1), $NF}')
  C3=$(cpu_ms)
  SAME=$(cmp -s /tmp/src /mnt/v/f && echo identical || echo DIFFERENT)
  echo "round $round: write $W ($((C1 - C0)) ms daemon CPU), read $R ($((C3 - C2)) ms daemon CPU), $SAME"
  rm -f /mnt/v/f
done
