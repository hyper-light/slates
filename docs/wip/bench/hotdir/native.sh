#!/bin/bash
# The hot-directory storm on Linux's own NFS client against a daemon in the same (privileged) container: the
# loopback round trip, with no Docker Desktop VM-to-host hop. Arguments: workers, files per worker. Run from the
# repository root with a Linux release build of slates-cli in the volume mounted at /target:
#   docker run --rm --privileged -v <linux-target>:/target:ro -v "$PWD/docs/wip/bench/hotdir":/bench:ro \
#     -v <output dir>:/out python:3.12-slim-trixie bash /bench/native.sh WORKERS FILES_PER_WORKER
set -u
W=${1:-16}; N=${2:-500}
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
slates --instance hot anchor --quick --shards 4 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 200); do slates --instance hot volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance hot bootstrap root >/dev/null 2>&1
slates --instance hot volume create hot --bounded 256MiB > /out/create.txt 2>&1
ID=$(awk '/^id/{print $2}' /out/create.txt)
[ -n "$ID" ] || { echo "create failed: $(cat /out/create.txt)"; exit 1; }
slates --instance hot export "$ID" > /out/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /out/export.txt); EXPORT=$(awk '/^export:/{print $2}' /out/export.txt)
mkdir -p /mnt/slates /mnt/ram
# Unmount on every exit path while the daemon still runs: a hard mount whose server is this container's own
# exiting daemon wedges the container's teardown forever (nfs4_proc_destroy_session under do_exit).
trap "umount -f -l /mnt/slates 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/slates || { echo "mount failed"; exit 1; }
mount -t tmpfs tmpfs /mnt/ram
echo "== slates NFSv4.2 (loopback)"
python3 /bench/hotdir.py /mnt/slates "$W" "$N"
awk '/^device .* mounted on \/mnt\/slates/{f=1} f&&/per-op statistics/{p=1;next} p&&NF==0{exit} p{print}' /proc/self/mountstats \
  | awk '$2>0 && ($1=="OPEN:"||$1=="WRITE:"||$1=="CLOSE:"||$1=="GETATTR:"||$1=="READ:") {printf "%s %d rtt %.3f\n", $1, $2, $8/$2}'
echo "== tmpfs (reference)"
python3 /bench/hotdir.py /mnt/ram "$W" "$N"
slates --instance hot status 2>/dev/null | grep -E "shard 0 nfs.local_p(50|99)_ns|delegation.granted"
umount /mnt/slates
