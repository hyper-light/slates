#!/bin/bash
# Condition 12's incumbent baseline: the same Linux NFSv4.2 client and the same mixed read-write workload (`mixed.py`)
# against slates and against the kernel's own NFS server (nfsd) exporting tmpfs, both in RAM, with plain tmpfs as the
# floor. Run in a privileged container with the Linux release build of slates at /target:
#   docker run --rm --privileged -v <linux-target>:/target:ro -v "$PWD/docs/wip/bench/mixed":/m:ro -v <out>:/out \
#     rust:1.98.0 bash /m/linux_vs_nfsd.sh [OPS_PER_WORKER] [LARGE_MIB]
# Every mount is removed and nfsd's threads stopped on every exit path, the slates mount while its daemon still runs.
set -u
OPS=${1:-400}; LARGE=${2:-64}
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common nfs-kernel-server python3 >/dev/null 2>&1
cleanup() {
  umount -f -l /mnt/slates /mnt/knfs 2>/dev/null
  exportfs -ua 2>/dev/null; rpc.nfsd 0 2>/dev/null
}
trap cleanup EXIT
# The kernel server, exporting a tmpfs as the NFSv4 pseudo-root (`fsid=0`), mounted as `/`.
mkdir -p /exp /mnt/knfs /mnt/slates /ram
mount -t tmpfs tmpfs /exp && mount -t tmpfs tmpfs /ram
mountpoint -q /proc/fs/nfsd || mount -t nfsd nfsd /proc/fs/nfsd
echo "/exp 127.0.0.1(rw,sync,no_subtree_check,no_root_squash,fsid=0)" > /etc/exports
rpcbind >/dev/null 2>&1; rpc.nfsd 8 && exportfs -ra && rpc.mountd >/dev/null 2>&1
mount -t nfs -o vers=4.2,proto=tcp,hard 127.0.0.1:/ /mnt/knfs || { echo "kernel nfs mount failed"; exit 1; }
# slates, its export mounted by the same client.
cp /target/release/slates /usr/local/bin/slates
slates --instance mx anchor --quick --shards 4 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 200); do slates --instance mx volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance mx bootstrap root >/dev/null 2>&1
ID=$(slates --instance mx volume create mx --dynamic 3GiB 2>&1 | awk '/^id/{print $2}')
[ -n "$ID" ] || { echo "create failed"; exit 1; }
slates --instance mx export "$ID" > /out/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /out/export.txt); EXPORT=$(awk '/^export:/{print $2}' /out/export.txt)
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard" "127.0.0.1:$EXPORT" /mnt/slates || { echo "slates mount failed"; exit 1; }
grep -E " /mnt/(slates|knfs) " /proc/mounts
for W in 1 4 16; do
  for target in slates knfs ram; do
    case $target in slates) D=/mnt/slates;; knfs) D=/mnt/knfs;; ram) D=/ram;; esac
    mkdir -p "$D/w$W"
    echo "== $target, $W workers"
    python3 /m/mixed.py "$D/w$W" "$W" "$OPS" "$LARGE"
  done
done
# The daemon's own account of the run: delegations, recalls and the calls per NFS operation.
slates --instance mx status --json > /out/status.json 2>/dev/null
