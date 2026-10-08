#!/bin/bash
# Condition 12, memory: what one file costs the daemon on a real tree, beside what the kernel's tmpfs pays for the same
# tree. A privileged container with the Linux release build at /target/release/slates copies TREE (default /usr) into
# a slates volume over the kernel's NFSv4.2 client and into tmpfs, and reports each side's growth:
#   docker run --rm --privileged -v <target-volume>:/target:ro -v "$PWD/docs/wip/bench/mem":/m:ro rust:1.98.0 \
#     -v <out>:/m-out bash /m/per_file.sh [TREE]
# slates' side is the proportional set size (smaps_rollup Pss: private pages plus each shared page divided by its
# sharers) of the anchor and the daemon together, so content kept in anchor-owned RAM (A-64) is counted once.
# tmpfs's side is the kernel's Shmem plus slab (inodes, dentries) from /proc/meminfo, after dropping clean caches.
# VOLUME_MAX sets the volume's dynamic maximum (default 8GiB); MALLOC_ENV is an environment assignment for the daemon
# (an allocator A/B, e.g. GLIBC_TUNABLES=...). Each run also prints the daemon's resident memory by mapping.
set -u
TREE=${1:-/usr}
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
pss_kib() { local total=0; for p in "$@"; do total=$((total + $(awk '/^Pss:/{print $2}' /proc/$p/smaps_rollup))); done; echo $total; }
detail() { awk '/^(Pss|Pss_Anon|Pss_File|Pss_Shmem|Private_Dirty):/{printf "%s %s ", $1, $2}' /proc/$1/smaps_rollup; }
meminfo_kib() { awk -v k="$1" '$1==k":"{print $2}' /proc/meminfo; }
kernel_kib() { sync; echo 3 > /proc/sys/vm/drop_caches; echo $(( $(meminfo_kib Shmem) + $(meminfo_kib Slab) )); }
FILES=$(find "$TREE" -xdev | wc -l); BYTES=$(find "$TREE" -xdev -type f -printf '%s\n' | awk '{s+=$1} END {print s}')
echo "tree $TREE: $FILES entries, $BYTES content bytes"
env ${MALLOC_ENV:-} slates --instance mem anchor --quick --shards 2 >/dev/null 2>&1 &
for _ in $(seq 1 300); do slates --instance mem volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance mem bootstrap root >/dev/null 2>&1
ID=$(slates --instance mem volume create mem --dynamic ${VOLUME_MAX:-8GiB} 2>&1 | awk '/^id/{print $2}')
slates --instance mem export "$ID" > /tmp/e.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /tmp/e.txt); EXPORT=$(awk '/^export:/{print $2}' /tmp/e.txt)
mkdir -p /mnt/v /ram; mount -t tmpfs tmpfs /ram
trap "umount -f -l /mnt/v 2>/dev/null; umount /ram 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/v || { echo MOUNT-FAILED; exit 1; }
PIDS=$(pgrep -f "slates --instance mem (anchor|daemon)" | tr '\n' ' ')
BEFORE=$(pss_kib $PIDS)
START=$(date +%s.%N); cp -a "$TREE/." /mnt/v/ ; sync; END=$(date +%s.%N)
AFTER=$(pss_kib $PIDS)
GROWN=$(( (AFTER - BEFORE) * 1024 ))
seconds() { awk -v a="$1" -v b="$2" 'BEGIN {printf "%.2f", b - a}'; }
echo "slates: copy $(seconds $START $END) s; Pss $BEFORE -> $AFTER KiB; growth $GROWN bytes"
echo "slates: $(( (GROWN - BYTES) / FILES )) bytes per entry beyond content"
REFERENCED=$(slates --instance mem status "$ID" --json 2>/dev/null | grep -o '"referenced_bytes":[0-9]*' | cut -d: -f2)
echo "slates: referenced $REFERENCED bytes ($(( (REFERENCED - BYTES) / FILES )) per entry of block rounding); $(( (GROWN - REFERENCED) / FILES )) bytes per entry beyond the referenced bytes"
slates --instance mem status --json > /m-out/status-mounted.json 2>/dev/null
# The client's opens and delegations end with its unmount; what the daemon still holds after it is the tree.
umount /mnt/v; sleep 2
UNMOUNTED=$(pss_kib $PIDS)
for p in $PIDS; do echo "  $(tr '\0' ' ' < /proc/$p/cmdline | cut -c1-40): $(detail $p)"; done
echo "slates: Pss after the unmount $UNMOUNTED KiB; $(( (UNMOUNTED * 1024 - REFERENCED) / FILES )) bytes per entry beyond the referenced bytes"
# A-105 purges an idle shard's free blocks on its reap ticks; read again once several ticks have passed.
sleep 15
IDLE=$(pss_kib $PIDS)
echo "slates: Pss 15 s later $IDLE KiB; $(( (IDLE * 1024 - REFERENCED) / FILES )) bytes per entry beyond the referenced bytes"
for p in $PIDS; do echo "  $(tr '\0' ' ' < /proc/$p/cmdline | cut -c1-40): $(detail $p)"; done
# The daemon's resident memory by mapping (the content object, the anchor segment, the heap), largest first.
D=$(pgrep -f "slates --instance mem daemon" | head -1)
awk '/^[0-9a-f]+-[0-9a-f]+ /{name=($6=="" ? "[anon]" : $6)} /^Pss:/{pss[name]+=$2} END {for (n in pss) if (pss[n] > 1024) printf "  %10d KiB  %s\n", pss[n], n}' /proc/$D/smaps | sort -rn | head -8
slates --instance mem status --json > /m-out/status-unmounted.json 2>/dev/null
pkill -f "slates --instance mem"; sleep 1
K0=$(kernel_kib); START=$(date +%s.%N); cp -a "$TREE/." /ram/; sync; END=$(date +%s.%N); K1=$(kernel_kib)
KGROWN=$(( (K1 - K0) * 1024 ))
echo "tmpfs: copy $(seconds $START $END) s; Shmem+Slab growth $KGROWN bytes; $(( (KGROWN - BYTES) / FILES )) bytes per entry beyond content"
