#!/bin/bash
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common python3 >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
slates --instance tr anchor --shards 2 >/dev/null 2>&1 &
for _ in $(seq 1 300); do slates --instance tr volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance tr bootstrap root >/dev/null 2>&1
ID=$(slates --instance tr volume create tr --dynamic 4GiB 2>&1 | awk '/^id/{print $2}')
slates --instance tr export "$ID" > /tmp/e.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /tmp/e.txt); EXPORT=$(awk '/^export:/{print $2}' /tmp/e.txt)
mkdir -p /mnt/v; trap "umount -f -l /mnt/v 2>/dev/null" EXIT
T=/sys/kernel/tracing; [ -d $T/events ] || mount -t tracefs nodev $T
echo > $T/trace
for e in nfs4/nfs4_sequence_done nfs4/nfs4_state_mgr nfs4/nfs4_state_mgr_failed nfs4/nfs4_write nfs4/nfs4_setup_sequence nfs4/nfs4_cb_sequence nfs4/nfs4_delegreturn_exit nfs4/nfs4_reclaim_delegation nfs4/nfs4_renew_async nfs4/nfs4_xdr_status; do
  [ -e $T/events/$e/enable ] && echo 1 > $T/events/$e/enable
done
echo 1 > $T/tracing_on
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/v || exit 1
python3 - <<'PY'
import os, time
chunk=os.urandom(1<<20); t=time.perf_counter()
for i in range(5):
    fd=os.open(f"/mnt/v/f{i}",os.O_WRONLY|os.O_CREAT|os.O_TRUNC,0o644); os.write(fd,chunk); a=time.perf_counter(); os.fsync(fd); print(f"fsync {i}: {(time.perf_counter()-a)*1000:.0f} ms"); os.close(fd)
PY
echo 0 > $T/tracing_on
grep -c . $T/trace
grep -oE "nfs4_[a-z_]+:" $T/trace | sort | uniq -c | sort -rn | head
grep -E "nfs4_sequence_done" $T/trace | grep -oE "status_flags=[^ ]+|error=[^ ]+" | sort | uniq -c | head
grep -E "nfs4_write|nfs4_sequence_done|nfs4_setup_sequence" $T/trace | tail -30 | sed -E "s/^ *[^ ]+ +\\[[0-9]+\\] [^ ]+ //" | cut -c1-200
