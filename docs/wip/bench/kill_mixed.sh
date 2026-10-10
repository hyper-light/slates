#!/bin/bash
# Condition 11 against every path A-126's deltas take: the daemon SIGKILLed KILLS times, GAP seconds apart, while a
# workload makes OPS random operations over FILES files on its NFSv4.2 volume: writes inside and past the end, truncates
# shorter and longer, appends, creates, unlinks and renames. Plain overwrites take the windowed patch; the rest fall back
# to whole images. Each operation opens, acts and closes its file; NFS close-to-open flushes on close, so each operation
# is durable by its end. The workload keeps a model of every file. After the storm the volume is remounted (a cold
# client cache) and the tree must equal the model, name for name and byte for byte. Run privileged with a Linux release
# build:
#   docker run --rm --privileged -v <linux slates>:/slates:ro -v "$PWD/docs/wip/bench":/b:ro \
#     [-e KILLS=20 -e GAP=0.5 -e OPS=6000 -e FILES=16 -e SEED=11] python:3.12-slim-trixie bash /b/kill_mixed.sh
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common >/dev/null 2>&1
cp /slates /usr/local/bin/slates
slates --instance km anchor --quick --shards 4 >/tmp/anchor.out 2>/tmp/anchor.err &
ANCHOR=$!
for _ in $(seq 1 300); do slates --instance km volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance km bootstrap root >/dev/null 2>&1
ID=$(slates --instance km volume create km --dynamic 2GiB 2>&1 | awk '/^id/{print $2}')
slates --instance km export "$ID" > /tmp/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /tmp/export.txt); EXPORT=$(awk '/^export:/{print $2}' /tmp/export.txt)
OPTIONS="vers=4.2,proto=tcp,port=$PORT,hard,timeo=600"
mkdir -p /mnt/v; trap "umount -f -l /mnt/v 2>/dev/null" EXIT
mount -t nfs -o "$OPTIONS" "127.0.0.1:$EXPORT" /mnt/v || { echo MOUNT-FAILED; exit 1; }
daemon_pid() { awk -v a="$ANCHOR" '/^PPid:/ && $2 == a {split(FILENAME, p, "/"); print p[3]; exit}' /proc/[0-9]*/status 2>/dev/null; }
python3 - /mnt/v /tmp/model.pickle <<'PY' &
import os, pickle, random, sys, time
root, model_path = sys.argv[1], sys.argv[2]
OPS, FILES = int(os.environ.get("OPS", "6000")), int(os.environ.get("FILES", "16"))
CHUNK = 64 << 10           # a Linux window
MAX = 4 << 20              # files stay under 4 MiB
rng = random.Random(int(os.environ.get("SEED", "11")))
model = {}                 # name -> bytearray
counts = {}
def path(name): return os.path.join(root, name)
failures = []
def act(kind, fn):
    counts[kind] = counts.get(kind, 0) + 1
    try:
        fn()
    except OSError as error:
        # Named, not swallowed: the operation, its file, and whether the file exists now. The model is not changed, so the
        # comparison after the storm shows what the volume holds against what the workload expected.
        failures.append(f"op {n} {kind} on {name}: {error.strerror}; the file now {'exists' if os.path.exists(path(name)) else 'is absent'}")
started = time.time()
for n in range(OPS):
    names = sorted(model)
    choice = rng.random()
    if not names or (choice < 0.08 and len(names) < FILES):
        name = f"f{n}"
        data = bytearray(rng.randbytes(rng.randrange(0, 3 * CHUNK)))
        def create():
            with open(path(name), "wb") as f: f.write(data)
            model[name] = data
        act("create", create)
        continue
    name = rng.choice(names)
    body = model[name]
    if choice < 0.55:
        at = rng.randrange(0, len(body) + CHUNK)
        block = rng.randbytes(rng.randrange(1, 2 * CHUNK))
        if at + len(block) > MAX: continue
        def write():
            fd = os.open(path(name), os.O_WRONLY)
            os.pwrite(fd, block, at); os.close(fd)
            if at > len(body): body.extend(b"\0" * (at - len(body)))
            body[at:at + len(block)] = block
        act("write", write)
    elif choice < 0.70:
        size = rng.randrange(0, min(MAX, len(body) + 2 * CHUNK))
        def truncate():
            os.truncate(path(name), size)
            if size < len(body): del body[size:]
            else: body.extend(b"\0" * (size - len(body)))
        act("truncate", truncate)
    elif choice < 0.82:
        block = rng.randbytes(rng.randrange(1, CHUNK))
        if len(body) + len(block) > MAX: continue
        def append():
            with open(path(name), "ab") as f: f.write(block)
            body.extend(block)
        act("append", append)
    elif choice < 0.90:
        def unlink():
            os.unlink(path(name)); del model[name]
        act("unlink", unlink)
    else:
        new = f"r{n}"
        def rename():
            os.rename(path(name), path(new)); model[new] = model.pop(name)
        act("rename", rename)
pickle.dump({k: bytes(v) for k, v in model.items()}, open(model_path, "wb"))
print(f"workload: {OPS} operations in {time.time() - started:.1f} s, {len(model)} files left; " + ", ".join(f"{k} {v}" for k, v in sorted(counts.items())))
for failure in failures: print("  FAILED", failure)
PY
WORKLOAD=$!
# Each daemon life's NFSv4 call-record counters (A-127), read just before the life is killed: a retry the life answered
# from the records the life before it left is counted here, so the storm shows the replays it took, not only the last
# life's.
replay_counts() {
  slates --instance km status --json 2>/dev/null | python3 -c '
import json, sys
counts = {}
for shard in json.load(sys.stdin)["shards"]:
    for r in shard.get("refusals", []):
        if r["kind"].startswith("nfs4.replay."):
            counts[r["kind"]] = counts.get(r["kind"], 0) + r["count"]
print(" ".join(f"{k}={v}" for k, v in sorted(counts.items())))' 2>/dev/null
}
sleep 1
for n in $(seq 1 "${KILLS:-20}"); do
  echo "life $n: $(replay_counts)" >> /tmp/lives.txt
  P=$(daemon_pid); [ -n "$P" ] && kill -9 "$P"
  sleep "${GAP:-0.5}"
done
wait $WORKLOAD || echo "WORKLOAD-FAILED"
python3 - /tmp/lives.txt <<'PY'
import sys
totals = {}
for line in open(sys.argv[1]):
    for field in line.split()[2:]:
        kind, _, count = field.partition("=")
        if count.isdigit():
            totals[kind] = totals.get(kind, 0) + int(count)
print("replay counters summed over the killed lives:", ", ".join(f"{k} {v}" for k, v in sorted(totals.items())) or "none")
PY
umount /mnt/v && mount -t nfs -o "$OPTIONS" "127.0.0.1:$EXPORT" /mnt/v || { echo REMOUNT-FAILED; exit 1; }
python3 - /mnt/v /tmp/model.pickle <<'PY'
import os, pickle, sys
root, model = sys.argv[1], pickle.load(open(sys.argv[2], "rb"))
present = sorted(n for n in os.listdir(root))
problems = []
if present != sorted(model):
    problems.append(f"names differ: extra {sorted(set(present) - set(model))[:5]}, missing {sorted(set(model) - set(present))[:5]}")
for name in sorted(set(present) & set(model)):
    got = open(os.path.join(root, name), "rb").read()
    if got != model[name]:
        first = next((i for i, (a, b) in enumerate(zip(got, model[name])) if a != b), min(len(got), len(model[name])))
        problems.append(f"{name}: got {len(got)} bytes, want {len(model[name])}, first difference at {first}")
total = sum(len(v) for v in model.values())
print(f"after the storm: {'the tree reads back identical' if not problems else 'DIFFERENT'} ({len(model)} files, {total} bytes)")
for p in problems[:10]: print("  ", p)
PY
echo "restarts: $(grep -c 'restarted' /tmp/anchor.err)"
slates --instance km status --json 2>/dev/null | python3 -c '
import json, sys
counts = {}
for shard in json.load(sys.stdin)["shards"]:
    for r in shard.get("refusals", []):
        k = r["kind"]
        if k.startswith("nfs4.") or k.startswith("nfs.") or "replay" in k or "recover" in k:
            counts[k] = counts.get(k, 0) + r["count"]
print("  counters:", ", ".join(f"{k} {v}" for k, v in sorted(counts.items())))' 
