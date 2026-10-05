#!/bin/bash
# The hot-directory storm against Linux's own kernel nfsd exporting a tmpfs, over loopback NFSv4.2: the like-for-like
# reference for ls-ab.sh: the same listing loop against the kernel nfsd.
set -u
W=${1:-16}; N=${2:-500}
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-kernel-server nfs-common >/dev/null 2>&1
mkdir -p /export /mnt/k
mount -t tmpfs tmpfs /export
mount -t nfsd nfsd /proc/fs/nfsd 2>&1 || { echo "no nfsd in this kernel"; exit 1; }
echo "/export 127.0.0.1(rw,no_root_squash,no_subtree_check,fsid=0)" > /etc/exports
echo 10 > /proc/fs/nfsd/nfsv4gracetime; echo 10 > /proc/fs/nfsd/nfsv4leasetime; rpcbind 2>&1; rpc.nfsd -N 3 8 2>&1; exportfs -ra 2>&1; rpc.mountd 2>&1; sleep 1
trap "umount -f -l /mnt/k 2>/dev/null; rpc.nfsd 0" EXIT
timeout 120 mount -t nfs -o vers=4.2,proto=tcp,hard,timeo=600 127.0.0.1:/ /mnt/k || { echo "mount failed"; exit 1; }
touch /mnt/k/.warm && rm /mnt/k/.warm  # waits out the grace period before timing
python3 /bench/lsonly.py /mnt/k 8000 0
python3 /bench/lsonly.py /mnt/k 0 300 a

awk '/^device .* mounted on \/mnt\/k/{f=1} f&&/per-op statistics/{p=1;next} p&&NF==0{exit} p{print}' /proc/self/mountstats | awk '$1=="READDIR:" {printf "READDIR ops %d bytes_recv/op %d rtt %.3f\n", $2, $6/$2, $8/$2}'
exit 0
