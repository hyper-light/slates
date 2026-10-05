#!/bin/bash
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-kernel-server nfs-common python3 iproute2 >/dev/null 2>&1
mkdir -p /export /mnt/k; mount -t tmpfs tmpfs /export
mount -t nfsd nfsd /proc/fs/nfsd 2>/dev/null
echo "/export 127.0.0.1(rw,no_root_squash,no_subtree_check,fsid=0)" > /etc/exports
echo 10 > /proc/fs/nfsd/nfsv4gracetime; echo 10 > /proc/fs/nfsd/nfsv4leasetime; rpcbind; rpc.nfsd -N 3 8; exportfs -ra; rpc.mountd; sleep 1
trap "umount -f -l /mnt/k 2>/dev/null; rpc.nfsd 0" EXIT
mount -t nfs -o vers=4.2,proto=tcp,hard,timeo=600 127.0.0.1:/ /mnt/k || exit 1
touch /mnt/k/.warm && rm /mnt/k/.warm
python3 - <<'PY'
import os, time
chunk=os.urandom(1<<20)
for i in range(5):
    fd=os.open(f"/mnt/k/f{i}",os.O_WRONLY|os.O_CREAT|os.O_TRUNC,0o644); os.write(fd,chunk); a=time.perf_counter(); os.fsync(fd); print(f"knfsd fsync {i}: {(time.perf_counter()-a)*1000:.1f} ms"); os.close(fd)
PY
dd if=/dev/zero of=/mnt/k/big bs=1M count=100 conv=fsync 2>&1 | tail -1
ss -tinm "( dport = :2049 )" | grep -oE "skmem:\([^)]*\)|tb[0-9]+" | head -2
