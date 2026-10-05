#!/bin/bash
# Condition 4 probe: device nodes and a planted setuid-root binary on a slates NFSv4.2 volume mounted by the kernel.
# Run as `realworld_native.sh` is (privileged; /target, /out). Add `nosuid,nodev` to the mount options to see A-94.
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
slates --instance es anchor --quick --shards 2 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 200); do slates --instance es volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance es bootstrap root >/dev/null 2>&1
ID=$(slates --instance es volume create es --dynamic 1GiB 2>&1 | awk '/^id/{print $2}')
slates --instance es export "$ID" > /out/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /out/export.txt); EXPORT=$(awk '/^export:/{print $2}' /out/export.txt)
mkdir -p /mnt/s; trap "umount -f -l /mnt/s 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/s || exit 1
grep " /mnt/s " /proc/mounts
cd /mnt/s
echo "== mknod char 1,1 (/dev/mem): $(mknod mem c 1 1 2>&1 && echo created)"
echo "== mknod block 7,0 (loop0):  $(mknod blk b 7 0 2>&1 && echo created)"
ls -l mem blk 2>&1
echo "== read the char device node: $(head -c 4 mem 2>&1 | od -An -tx1 | head -1)"
cp /usr/bin/id ./suid-id && chown root:root suid-id && chmod 4755 suid-id
ls -l suid-id
echo "== setuid as nobody: $(su -s /bin/sh nobody -c '/mnt/s/suid-id -u' 2>&1)"
echo "== setgid dir: $(mkdir sg && chmod 2775 sg && stat -c %A sg)"
