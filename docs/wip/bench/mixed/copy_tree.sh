#!/bin/bash
# Condition 12: a real tree copied with `cp -a` (many files, real sizes) into slates and into the kernel's own NFS server
# (nfsd exporting tmpfs, `sync` and `async`), both through the same Linux NFSv4.2 client, with tmpfs itself as the floor. Each target is
# copied RUNS times, interleaved, into a fresh directory that is removed (untimed) before the next copy. Run in a privileged container with the Linux release build:
#   docker run --rm --privileged -v <target-volume>:/target:ro -v "$PWD/docs/wip/bench/mixed":/m:ro rust:1.98 \
#     bash /m/copy_tree.sh [TREE] [RUNS]
set -u
TREE=${1:-/usr}; RUNS=${2:-3}
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common nfs-kernel-server >/dev/null 2>&1
cleanup() { umount -f -l /mnt/slates /mnt/knfs /mnt/knfsa 2>/dev/null; exportfs -ua 2>/dev/null; rpc.nfsd 0 2>/dev/null; }
trap cleanup EXIT
mkdir -p /exp /expa /mnt/knfs /mnt/knfsa /mnt/slates /ram
mount -t tmpfs tmpfs /exp && mount -t tmpfs tmpfs /expa && mount -t tmpfs tmpfs /ram
mountpoint -q /proc/fs/nfsd || mount -t nfsd nfsd /proc/fs/nfsd
# nfsd exported both ways: `sync` (stable before each reply, the durability slates' replies also give) and `async` (its
# fastest). The v4 pseudo-root holds both.
echo "/exp 127.0.0.1(rw,sync,no_subtree_check,no_root_squash,fsid=0,crossmnt)" > /etc/exports
echo "/exp/a 127.0.0.1(rw,async,no_subtree_check,no_root_squash)" >> /etc/exports
mkdir -p /exp/a && mount --bind /expa /exp/a
# A fresh nfsd refuses new opens for its v4 grace period (90 s by default), which a client sits out by retrying; shorten
# it and wait it out, or the first copy measures the grace period (2026-10-08: 93 s against 24-29 s once past it).
echo 10 > /proc/fs/nfsd/nfsv4gracetime 2>/dev/null; echo 10 > /proc/fs/nfsd/nfsv4leasetime 2>/dev/null
rpcbind >/dev/null 2>&1; rpc.nfsd 8 && exportfs -ra && rpc.mountd >/dev/null 2>&1
mount -t nfs -o vers=4.2,proto=tcp,hard 127.0.0.1:/ /mnt/knfs || { echo "kernel nfs mount failed"; exit 1; }
mount -t nfs -o vers=4.2,proto=tcp,hard 127.0.0.1:/a /mnt/knfsa || { echo "kernel nfs async mount failed"; exit 1; }
sleep 12; touch /mnt/knfs/.warm /mnt/knfsa/.warm && rm -f /mnt/knfs/.warm /mnt/knfsa/.warm
cp /target/release/slates /usr/local/bin/slates
slates --instance ct anchor --quick --shards 4 >/dev/null 2>&1 &
for _ in $(seq 1 300); do slates --instance ct volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance ct bootstrap root >/dev/null 2>&1
ID=$(slates --instance ct volume create ct --dynamic 16GiB 2>&1 | awk '/^id/{print $2}')
slates --instance ct export "$ID" > /tmp/e.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /tmp/e.txt); EXPORT=$(awk '/^export:/{print $2}' /tmp/e.txt)
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard" "127.0.0.1:$EXPORT" /mnt/slates || { echo "slates mount failed"; exit 1; }
echo "tree $TREE: $(find "$TREE" -xdev | wc -l) entries, $(du -sb "$TREE" | cut -f1) bytes; load $(cut -d' ' -f1-3 /proc/loadavg)"
for run in $(seq 1 "$RUNS"); do
  for target in slates knfs knfs-async ram; do
    case $target in slates) D=/mnt/slates;; knfs) D=/mnt/knfs;; knfs-async) D=/mnt/knfsa;; ram) D=/ram;; esac
    START=$(date +%s%N); cp -a "$TREE" "$D/r$run"; sync -f "$D"; END=$(date +%s%N)
    echo "run $run $target: $(( (END - START) / 1000000 )) ms"
    # Removed untimed, so one copy at a time is held: three targets' copies at once outgrew a 7.6 GiB VM (2026-10-08).
    rm -rf "${D:?}/r$run"; sync -f "$D"
  done
done
slates --instance ct status --json > /tmp/status.json 2>/dev/null
grep -o '"nfs.local_p99_ns","value":[0-9]*\|"nfs.local_p50_ns","value":[0-9]*' /tmp/status.json | head -2
