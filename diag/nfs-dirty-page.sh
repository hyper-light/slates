#!/bin/bash
# Diagnostics, never merged: through the product's own NFSv3 mount on macOS, does a page dirtied through
# a shared map and not yet synced undo another path's later replacement of the file when it is written back?
set -u
S="$SLATES_BIN"
W="$SLATES_WORK"
I=a186dirty-$$
M=$W/mnt
mkdir -p "$M"
"$S" --instance "$I" anchor --quick --shards 1 2> "$W/anchor-$I.err" &
A=$!
cleanup() {
  # Unmount first, always; a daemon stopped under a live macOS NFS mount has panicked this kernel.
  if mount | grep -q " on $M "; then
    "$S" --instance "$I" unmount "$M" || umount "$M"
  fi
  if mount | grep -q " on $M "; then
    echo "STILL MOUNTED at $M: the daemon (pid $A) is left running"
    return
  fi
  kill "$A" 2>/dev/null
  wait "$A" 2>/dev/null
  echo "unmounted, daemon stopped"
}
trap cleanup EXIT
for _ in $(seq 1 600); do "$S" --instance "$I" volume list >/dev/null 2>&1 && break; sleep 0.2; done
"$S" --instance "$I" bootstrap root >/dev/null
ID=$("$S" --instance "$I" volume create maps --bounded 8MiB | awk '/^id:/{print $2}')
ATT=$("$S" --instance "$I" attach "$ID" --write | awk '/^attachment:/{print $2}')
PAGE=$(getconf PAGESIZE)
echo "volume $ID attachment $ATT page $PAGE"
head -c "$PAGE" /dev/zero | tr '\0' 'A' | "$S" --instance "$I" write "$ID" "$ATT" map.bin >/dev/null
"$S" --instance "$I" mount "$ID" "$M" || exit 1
mount | grep " on $M "
python3 -I - "$M/map.bin" "$PAGE" "$S" "$I" "$ID" "$ATT" <<'PY'
import mmap, os, subprocess, sys
path, page, s, inst, vid, att = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6]
f = open(path, "r+b")
m = mmap.mmap(f.fileno(), page, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
print("map before:", m[0:8])
m[0:1] = b"Z"
subprocess.run([s, "--instance", inst, "write", vid, att, "map.bin"], input=b"C" * page, check=True, capture_output=True)
os.stat(path)
print("map after the other path's replace and a stat:", m[0:8])
m.flush()
m.close()
f.close()
out = subprocess.run([s, "--instance", inst, "read", vid, "map.bin"], capture_output=True, check=True).stdout
print("the file after msync, read through slates:", out[:8], "C:", out.count(b"C"), "A:", out.count(b"A"), "Z:", out.count(b"Z"), "len:", len(out))
PY
