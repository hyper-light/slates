#!/bin/bash
# Condition 11: the daemon in a memory-capped container (cgroup v2 memory.max, as a pod limit), a real workload
# writing past what the cap allows over the kernel's NFSv4.2 client. Expect typed refusals (ENOSPC/EDQUOT), no panic,
# no OOM kill, status still answering, earlier files intact, and space back after a delete.
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common python3 >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
echo "memory.max: $(cat /sys/fs/cgroup/memory.max)"
slates --instance mp anchor --shards 2 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 300); do slates --instance mp volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance mp bootstrap root >/dev/null 2>&1
ID=$(slates --instance mp volume create mp --dynamic 4GiB 2>&1 | awk '/^id/{print $2}')
[ -n "$ID" ] || { echo "create failed: $(slates --instance mp volume create mp --dynamic 4GiB 2>&1)"; exit 1; }
slates --instance mp export "$ID" > /out/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /out/export.txt); EXPORT=$(awk '/^export:/{print $2}' /out/export.txt)
mkdir -p /mnt/v; trap "umount -f -l /mnt/v 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/v || exit 1
python3 - <<'PY'
import os, errno, hashlib, time
import sys
d="/mnt/v"; chunk=os.urandom(1<<20); written=0; first_err=None; hashes={}
t0=time.time()
for i in range(int(os.environ.get("FILES","4096"))):
    p=f"{d}/f{i:05d}"
    try:
        with open(p,"wb") as f:
            f.write(chunk); f.flush(); os.fsync(f.fileno())
        hashes[p]=hashlib.sha256(chunk).hexdigest(); written+=1
    except OSError as e:
        first_err=(i, errno.errorcode.get(e.errno,e.errno)); break
print(f"wrote {written} MiB before {first_err} in {time.time()-t0:.1f}s")
bad=sum(1 for p,h in list(hashes.items())[:200] if hashlib.sha256(open(p,'rb').read()).hexdigest()!=h)
print(f"verified first {min(200,len(hashes))} files: {bad} mismatched")
for p in list(hashes)[:written//2]: os.remove(p)
try:
    with open(f"{d}/after-delete","wb") as f: f.write(chunk*8); f.flush(); os.fsync(f.fileno())
    print("after deleting half: an 8 MiB write succeeded")
except OSError as e:
    print(f"after deleting half: write refused {errno.errorcode.get(e.errno)}")
PY
awk '/^device .* mounted on \/mnt\/v/{f=1} f&&/per-op statistics/{p=1;next} p&&NF==0{exit} p{print}' /proc/self/mountstats | awk '$2>0 {printf "%s ops %d avg_rtt %.2f ms avg_exe %.2f ms bytes_sent/op %d\n", $1, $2, $8/$2, $9/$2, $5/$2}'
echo "daemon alive: $(pgrep -f 'slates --instance mp daemon' >/dev/null && echo yes || echo NO)"
echo "anchor restarts: $(grep -c 'restart' /out/anchor.err)"
echo "panics: $(grep -ci 'panicked' /out/anchor.err)"
echo "oom kills: $(cat /sys/fs/cgroup/memory.events | grep oom_kill)"
slates --instance mp status 2>&1 | grep -iE "nfs|write|status|retry|jukebox" | head -60
slates --instance mp status 2>&1 | grep -iE "^generation|budget|refused.*budget|BudgetExceeded|committed_bytes|reserve" | head -8
