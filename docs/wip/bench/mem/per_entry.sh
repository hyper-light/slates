#!/bin/bash
# Condition 12, memory: what one empty file costs, with no content to round, split between the anchor and the daemon.
# A privileged container with the Linux release build at /target/release/slates creates COUNT empty files (default
# 100,000) in 100 directories over the kernel's NFSv4.2 client, and the same on tmpfs:
#   docker run --rm --privileged -v <target-volume>:/target:ro -v "$PWD/docs/wip/bench/mem":/m:ro rust:1.98.0 \
#     bash /m/per_entry.sh [COUNT]
set -u
COUNT=${1:-100000}
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common python3 >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
pss_kib() { awk '/^Pss:/{print $2}' /proc/$1/smaps_rollup; }
detail() { awk '/^(Rss|Pss|Pss_Anon|Pss_File|Pss_Shmem|Shared_Clean|Private_Dirty):/{printf "%s %s ", $1, $2}' /proc/$1/smaps_rollup; }
kernel_kib() { sync; echo 3 > /proc/sys/vm/drop_caches; awk '$1=="Shmem:"||$1=="Slab:"{s+=$2} END {print s}' /proc/meminfo; }
make_files() { python3 - "$1" "$COUNT" <<'PY'
import os, sys, time
root, count = sys.argv[1], int(sys.argv[2])
for d in range(100):
    os.mkdir(os.path.join(root, f"d{d}"))
start = time.perf_counter()
for i in range(count):
    os.close(os.open(os.path.join(root, f"d{i % 100}", f"f{i}"), os.O_CREAT | os.O_WRONLY, 0o644))
print(f"{count} creates in {time.perf_counter() - start:.2f} s")
PY
}
slates --instance pe anchor --quick --shards 2 >/dev/null 2>&1 &
for _ in $(seq 1 300); do slates --instance pe volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance pe bootstrap root >/dev/null 2>&1
ID=$(slates --instance pe volume create pe --dynamic 8GiB 2>&1 | awk '/^id/{print $2}')
slates --instance pe export "$ID" > /tmp/e.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /tmp/e.txt); EXPORT=$(awk '/^export:/{print $2}' /tmp/e.txt)
mkdir -p /mnt/v /ram; mount -t tmpfs tmpfs /ram
trap "umount -f -l /mnt/v 2>/dev/null; umount /ram 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/v || { echo MOUNT-FAILED; exit 1; }
ANCHOR=$(pgrep -f "slates --instance pe anchor"); DAEMON=$(pgrep -f "slates --instance pe daemon")
A0=$(pss_kib $ANCHOR); D0=$(pss_kib $DAEMON)
echo "before: anchor $(detail $ANCHOR)"; echo "before: daemon $(detail $DAEMON)"
make_files /mnt/v
umount /mnt/v; sleep 2
A1=$(pss_kib $ANCHOR); D1=$(pss_kib $DAEMON)
echo "after: anchor $(detail $ANCHOR)"; echo "after: daemon $(detail $DAEMON)"
echo "slates: anchor $(( (A1 - A0) * 1024 / COUNT )) + daemon $(( (D1 - D0) * 1024 / COUNT )) bytes per empty file"
# An idle shard gives back what it no longer needs on its reap ticks (A-105, A-117): read again once several passed.
sleep 15
A2=$(pss_kib $ANCHOR); D2=$(pss_kib $DAEMON)
echo "idle: daemon $(detail $DAEMON)"
echo "slates idle: anchor $(( (A2 - A0) * 1024 / COUNT )) + daemon $(( (D2 - D0) * 1024 / COUNT )) bytes per empty file"
slates --instance pe status --json > /tmp/status.json 2>/dev/null; grep -o '"committed_bytes":[0-9]*' /tmp/status.json | head -2
pkill -f "slates --instance pe"; sleep 1
K0=$(kernel_kib); make_files /ram; K1=$(kernel_kib)
echo "tmpfs: $(( (K1 - K0) * 1024 / COUNT )) bytes per empty file (Shmem + slab)"
