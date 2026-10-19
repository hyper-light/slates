#!/bin/bash
# Condition 11, supervision under a kill storm (A-119): the daemon SIGKILLed eight times, 100 ms apart (faster than it
# starts, so its supervisor sees a crash loop), while a file sits on its mounted NFSv4.2 volume. The anchor must keep
# restarting it (backing off, never giving up), and the mount must serve the file intact afterwards. Run privileged
# with the Linux release build at /target/release/slates and an output directory at /out:
#   docker run --rm --privileged -v <target-volume>:/target:ro -v "$PWD/docs/wip/bench":/b:ro -v <out>:/out rust:1.98 \
#     [-e KILLS=40 -e GAP=0.02] bash /b/kill_storm.sh
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
slates --instance ks anchor --quick --shards 4 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 300); do slates --instance ks volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance ks bootstrap root >/dev/null 2>&1
ID=$(slates --instance ks volume create ks --dynamic 1GiB 2>&1 | awk '/^id/{print $2}')
slates --instance ks export "$ID" > /out/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /out/export.txt); EXPORT=$(awk '/^export:/{print $2}' /out/export.txt)
mkdir -p /mnt/v; trap "umount -f -l /mnt/v 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/v || { echo MOUNT-FAILED; exit 1; }
# Durable by the file's own fsync, never a global `sync`, which waits on every filesystem of the host, and one stuck
# elsewhere hangs it forever (2026-10-08).
head -c 8388608 /dev/urandom > /tmp/payload; dd if=/tmp/payload of=/mnt/v/payload bs=1M conv=fsync status=none
# KILLS kills GAP seconds apart (default 8 and 0.1); enough of them out-kill the derived restart allowance (the
# recovery budget over the longest start), which is the crash loop A-119 backs off from rather than giving up on.
for n in $(seq 1 "${KILLS:-8}"); do
  P=$(pgrep -f "slates --instance ks daemon" | head -1); [ -n "$P" ] && kill -9 "$P"; sleep "${GAP:-0.1}"
done
START=$(date +%s.%N)
cmp -s /tmp/payload /mnt/v/payload && SAME=identical || SAME=DIFFERENT
echo "after the storm: the file reads back $SAME in $(awk -v s="$START" -v e="$(date +%s.%N)" 'BEGIN {printf "%.2f", e - s}') s"
echo "anchor alive: $(pgrep -f 'slates --instance ks anchor' >/dev/null && echo yes || echo NO)"
grep -cE "crash loop" /out/anchor.err | sed 's/^/crash loops recorded: /'
grep -E "giving up|crash loop" /out/anchor.err | tail -3
