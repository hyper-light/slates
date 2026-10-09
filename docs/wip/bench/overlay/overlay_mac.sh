#!/bin/bash
# Condition 5 on macOS's own NFS client (loopback, mount_nfs, no sudo): an overlay volume over a real tree, read,
# changed, its change set planned by `slates land`, then an outsider's edits of the disk beneath it. The tree is a copy
# kept in the build output (never /tmp, A-50). The mount is always removed before the anchor is stopped: a daemon
# killed under a live macOS kernel NFS mount has panicked this machine (memory: macos-nfs-kill-test-panics).
#   bash overlay_mac.sh SLATES_BINARY TREE OUT_DIR
# Keep OUT_DIR outside any directory shared into a Docker container: the VM's file sharing holds directories in the
# mount open, and its unmount then reports busy.
set -u
B=$1; TREE=$2; OUT=$3
INST="ovmac-$$"
mkdir -p "$OUT/mnt"
rm -rf "$OUT/base" "$OUT/copy" && cp -R "$TREE" "$OUT/base" && cp -R "$OUT/base" "$OUT/copy"
"$B" --instance "$INST" anchor --shards 4 > "$OUT/anchor.out" 2> "$OUT/anchor.err" &
ANCHOR=$!
cleanup() {
  "$B" --instance "$INST" unmount "$OUT/mnt" > "$OUT/unmount.txt" 2>&1 || umount "$OUT/mnt" 2>/dev/null
  # A busy mount (another process holds a directory in it open) is forced off while the daemon still serves it,
  # which is safe; only a daemon gone under a live mount is not. Measured 2026-10-08: Docker Desktop's file sharing
  # held directories open in a mount kept inside a tree shared into a container.
  mount | grep -q "$OUT/mnt" && umount -f "$OUT/mnt" 2>/dev/null
  if mount | grep -q "$OUT/mnt"; then
    echo "STILL MOUNTED: leaving the anchor running (pid $ANCHOR); unmount $OUT/mnt by hand first" >&2
    return
  fi
  kill "$ANCHOR" 2>/dev/null; wait "$ANCHOR" 2>/dev/null
}
trap cleanup EXIT
ms() { python3 -c 'import sys; print(round((float(sys.argv[2]) - float(sys.argv[1])) * 1000))' "$1" "$2"; }
now() { python3 -c 'import time; print(time.time())'; }
for _ in $(seq 1 300); do "$B" --instance "$INST" volume list > /dev/null 2>&1 && break; sleep 0.1; done
"$B" --instance "$INST" bootstrap root > /dev/null 2>&1
"$B" --instance "$INST" volume create ov --dynamic 4GiB --base "$OUT/base" > "$OUT/create.txt" 2>&1
ID=$(awk '/^id/{print $2}' "$OUT/create.txt")
[ -n "$ID" ] || { echo "create failed: $(cat "$OUT/create.txt")"; exit 1; }
"$B" --instance "$INST" mount "$ID" "$OUT/mnt" > "$OUT/mount.txt" 2>&1 || { echo "mount failed: $(cat "$OUT/mount.txt")"; exit 1; }
FILES=$(find "$OUT/base" -type f | wc -l | tr -d ' ')
echo "tree $TREE: $FILES files, $(du -sk "$OUT/base" | cut -f1) KiB; load $(sysctl -n vm.loadavg)"
A=$(now); find "$OUT/mnt" -type f -print0 | xargs -0 cat > /dev/null; B2=$(now)
find "$OUT/base" -type f -print0 | xargs -0 cat > /dev/null; C=$(now)
echo "cold read: slates $(ms "$A" "$B2") ms, host $(ms "$B2" "$C") ms"
# The same 18 changes as overlay_diff.sh, on files picked by a fixed stride.
PICKS=(); while IFS= read -r f; do PICKS+=("$f"); done < <(cd "$OUT/base" && find . -type f -size +0 | sort | awk -v n="$FILES" 'NR % int(n / 14 + 1) == 1' | head -13)
UNTOUCHED=$(cd "$OUT/base" && find . -type f -size +0 | sort | awk -v n="$FILES" 'NR % int(n / 14 + 1) == 2' | head -1)
change() {
  local root=$1
  for k in 0 1 2 3 4 5 6 7 8 9; do printf 'appended\n' >> "$root/${PICKS[$k]}"; done
  for k in 1 2 3 4 5; do printf 'created %s\n' "$k" > "$root/created-$k"; done
  for k in 10 11 12; do rm -f "$root/${PICKS[$k]}"; done
}
change "$OUT/mnt"; change "$OUT/copy"
A=$(now); "$B" --instance "$INST" land "$ID" "$OUT/base" --json > "$OUT/plan.json" 2>&1; B2=$(now)
diff -rq "$OUT/base" "$OUT/copy" > "$OUT/diff.txt" 2>&1; C=$(now)
echo "land plan $(ms "$A" "$B2") ms: $(python3 -c 'import json, sys; s = json.load(open(sys.argv[1]))["summary"]; print(" ".join(a["action"] + "=" + str(a["count"]) for a in s["by_action"]))' "$OUT/plan.json") | diff -rq $(ms "$B2" "$C") ms, $(wc -l < "$OUT/diff.txt" | tr -d ' ') lines"
# The outsider: a file the volume replaced (drift; the plan must name a conflict) and one it never touched (the mount
# must show the new bytes). Read once through the mount first, so the client holds the old bytes.
cat "$OUT/mnt/$UNTOUCHED" > /dev/null
printf 'outsider\n' >> "$OUT/base/${PICKS[0]}"; printf 'outsider\n' >> "$OUT/base/$UNTOUCHED"
A=$(now); seen=no
for _ in $(seq 1 400); do cmp -s "$OUT/mnt/$UNTOUCHED" "$OUT/base/$UNTOUCHED" && { seen=yes; break; }; sleep 0.05; done
B2=$(now)
echo "outsider edit of an untouched file seen through the mount: $seen after $(ms "$A" "$B2") ms"
echo "drifted: $("$B" --instance "$INST" status "$ID" --drift 2>&1 | tr '\n' ' ')"
"$B" --instance "$INST" land "$ID" "$OUT/base" --json > "$OUT/plan2.json" 2>&1
echo "land plan after the outsider: conflicts $(python3 -c 'import json, sys; print(json.load(open(sys.argv[1])).get("conflicts"))' "$OUT/plan2.json")"
