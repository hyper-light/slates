#!/bin/bash
set -u
B=$1; OUT=$2; INST="steps-$$"; mkdir -p "$OUT/mnt"
"$B" --instance "$INST" anchor --shards 1 > "$OUT/anchor.out" 2> "$OUT/anchor.err" &
ANCHOR=$!
cleanup() {
  "$B" --instance "$INST" unmount "$OUT/mnt" > /dev/null 2>&1 || umount "$OUT/mnt" 2>/dev/null
  if mount | grep -q "$OUT/mnt"; then echo "STILL MOUNTED; anchor $ANCHOR left running" >&2; return; fi
  kill "$ANCHOR" 2>/dev/null; wait "$ANCHOR" 2>/dev/null
}
trap cleanup EXIT
for _ in $(seq 1 300); do "$B" --instance "$INST" volume list > /dev/null 2>&1 && break; sleep 0.1; done
"$B" --instance "$INST" bootstrap root > /dev/null 2>&1
"$B" --instance "$INST" volume create steps --bounded 256MiB > "$OUT/create.txt" 2>&1
ID=$(awk '/^id/{print $2}' "$OUT/create.txt")
"$B" --instance "$INST" mount "$ID" "$OUT/mnt" > "$OUT/mount.txt" 2>&1 || { cat "$OUT/mount.txt"; exit 1; }
python3 -c 'import os,sys; [open(os.path.join(sys.argv[1],n),"wb").write(b"z"*4096) for n in ("base","victim")]' "$OUT/mnt"
sleep 2
counts() { "$B" --instance "$INST" status --json | python3 -c '
import json,sys
names="null getattr setattr lookup access readlink read write create mkdir symlink mknod remove rmdir rename link readdir readdirplus fsstat fsinfo pathconf commit".split()
t=[0]*22
for s in json.load(sys.stdin)["shards"]:
    for i,c in enumerate(s.get("nfs_calls",[])): t[i]+=c
print(" ".join(f"{n}={c}" for n,c in zip(names,t)))'; }
for step in stat open_read_close overwrite create_close create_write_close unlink; do
  sleep 4  # past the client's 1 s attribute cache, so each step starts cold
  a=$(counts); python3 "$(dirname "$0")/steps.py" "$OUT/mnt" "$step"; b=$(counts)
  python3 - "$step" "$a" "$b" <<'PY'
import sys
step,a,b=sys.argv[1],dict(x.split("=") for x in sys.argv[2].split()),dict(x.split("=") for x in sys.argv[3].split())
print(f"{step:20s}", {k:int(b[k])-int(a[k]) for k in b if int(b[k])-int(a[k])})
PY
done
