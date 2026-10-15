#!/bin/bash
# RPCs per client operation through macOS's NFS client, counted by the daemon (`nfs_calls` in status).
set -u
B=$1; OUT=$2; N=${3:-200}
INST="rpc-$$"; mkdir -p "$OUT/mnt"
"$B" --instance "$INST" anchor --shards 4 > "$OUT/anchor.out" 2> "$OUT/anchor.err" &
ANCHOR=$!
cleanup() {
  "$B" --instance "$INST" unmount "$OUT/mnt" > /dev/null 2>&1 || umount "$OUT/mnt" 2>/dev/null
  if mount | grep -q "$OUT/mnt"; then echo "STILL MOUNTED; anchor $ANCHOR left running" >&2; return; fi
  kill "$ANCHOR" 2>/dev/null; wait "$ANCHOR" 2>/dev/null
}
trap cleanup EXIT
for _ in $(seq 1 300); do "$B" --instance "$INST" volume list > /dev/null 2>&1 && break; sleep 0.1; done
"$B" --instance "$INST" bootstrap root > /dev/null 2>&1
"$B" --instance "$INST" volume create rpc --bounded 1GiB > "$OUT/create.txt" 2>&1
ID=$(awk '/^id/{print $2}' "$OUT/create.txt")
"$B" --instance "$INST" mount "$ID" "$OUT/mnt" > "$OUT/mount.txt" 2>&1 || { cat "$OUT/mount.txt"; exit 1; }
counts() { "$B" --instance "$INST" status --json | python3 -c '
import json,sys
names="null getattr setattr lookup access readlink read write create mkdir symlink mknod remove rmdir rename link readdir readdirplus fsstat fsinfo pathconf commit".split()
total=[0]*22
for shard in json.load(sys.stdin)["shards"]:
    for i,c in enumerate(shard.get("nfs_calls",[])): total[i]+=c
print(json.dumps(dict(zip(names,total))))'; }
counts > "$OUT/before.json"
python3 "$(dirname "$0")/creates.py" "$OUT/mnt" "$N"
counts > "$OUT/after.json"
python3 - "$OUT/before.json" "$OUT/after.json" "$N" <<'PY'
import json,sys
a,b,n=json.load(open(sys.argv[1])),json.load(open(sys.argv[2])),int(sys.argv[3])
per={k:(b[k]-a[k])/n for k in b if b[k]-a[k]>0}
print("RPCs per create (daemon-counted):", {k:round(v,2) for k,v in sorted(per.items(), key=lambda kv:-kv[1])})
print("total per create:", round(sum(per.values()),2))
PY
