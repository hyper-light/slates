#!/bin/bash
# Mixed read-write tail latency on macOS's own NFS client against a slates volume (loopback, mount_nfs, no sudo),
# with the scratch APFS directory as the reference. The mount is always removed before the anchor is stopped: a
# daemon killed under a live macOS kernel NFS mount has panicked this machine (memory: macos-nfs-kill-test-panics).
#   bash mixed_mac.sh SLATES_BINARY OUT_DIR OPS_PER_WORKER LARGE_MIB
set -u
B=$1; OUT=$2; OPS=${3:-400}; LARGE=${4:-64}
HERE=$(cd "$(dirname "$0")" && pwd)
INST="mixed-$$"
mkdir -p "$OUT/mnt" "$OUT/ref"
"$B" --instance "$INST" anchor --shards 4 > "$OUT/anchor.out" 2> "$OUT/anchor.err" &
ANCHOR=$!
cleanup() {
  "$B" --instance "$INST" unmount "$OUT/mnt" > "$OUT/unmount.txt" 2>&1 || umount "$OUT/mnt" 2>/dev/null
  # Only once nothing is mounted from this daemon is it stopped.
  if mount | grep -q "$OUT/mnt"; then
    echo "STILL MOUNTED: leaving the anchor running (pid $ANCHOR); unmount $OUT/mnt by hand first" >&2
    return
  fi
  kill "$ANCHOR" 2>/dev/null; wait "$ANCHOR" 2>/dev/null
}
trap cleanup EXIT
for _ in $(seq 1 300); do "$B" --instance "$INST" volume list > /dev/null 2>&1 && break; sleep 0.1; done
"$B" --instance "$INST" bootstrap root > "$OUT/bootstrap.txt" 2>&1
"$B" --instance "$INST" volume create mixed --bounded 2GiB > "$OUT/create.txt" 2>&1
ID=$(awk '/^id/{print $2}' "$OUT/create.txt")
[ -n "$ID" ] || { echo "create failed: $(cat "$OUT/create.txt")"; exit 1; }
"$B" --instance "$INST" mount "$ID" "$OUT/mnt" > "$OUT/mount.txt" 2>&1 || { echo "mount failed: $(cat "$OUT/mount.txt")"; exit 1; }
for W in 1 4 16; do
  mkdir -p "$OUT/mnt/w$W" "$OUT/ref/w$W"
  echo "== slates NFS (macOS client), $W workers"
  python3 "$HERE/mixed.py" "$OUT/mnt/w$W" "$W" "$OPS" "$LARGE"
  echo "== APFS scratch (reference), $W workers"
  python3 "$HERE/mixed.py" "$OUT/ref/w$W" "$W" "$OPS" "$LARGE"
done
"$B" --instance "$INST" status 2>/dev/null | grep -E "nfs.local_p(50|99)_ns" | head -4
rm -rf "$OUT/ref"
