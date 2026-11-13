#!/bin/bash
# GAPS 2026-10-06 "two fsync-acknowledged files came back empty after daemon kills on the macOS NFS mount", asked of the
# Linux kernel client instead: the writer of that run (files of 100 B to 300 KB written whole and fsynced, each hashed
# outside the volume only after its fsync returned; past 2,000 files an old one deleted every other step) for DURATION
# seconds while the daemon is SIGKILLed KILLS times at random 0.4-1.6 s intervals. Afterwards the volume is remounted
# (a cold client cache) and every recorded file must read back with its hash, every deleted name absent.
#   docker run --rm --privileged -v <linux slates>:/slates:ro -v "$PWD/docs/wip/bench":/b:ro [-e VERS=3|4.2 -e KILLS=15 -e DURATION=30]
#     python:3.13 bash /b/kill_fsync.sh
set -u
VERS=${VERS:-3}; KILLS=${KILLS:-15}; DURATION=${DURATION:-30}
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common >/dev/null 2>&1
cp /slates /usr/local/bin/slates
slates --instance kf anchor --quick --shards 4 >/tmp/anchor.out 2>/tmp/anchor.err &
ANCHOR=$!
for _ in $(seq 1 300); do slates --instance kf volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance kf bootstrap root >/dev/null 2>&1
ID=$(slates --instance kf volume create kf --dynamic 2GiB 2>&1 | awk '/^id/{print $2}')
slates --instance kf export "$ID" > /tmp/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /tmp/export.txt); EXPORT=$(awk '/^export:/{print $2}' /tmp/export.txt)
if [ "$VERS" = 3 ]; then
  MOUNTPORT=$(awk '/^mount_port:/{print $2}' /tmp/export.txt)
  OPTIONS="vers=3,proto=tcp,port=$PORT,mountport=${MOUNTPORT:-$PORT},mountproto=tcp,nolock,hard,timeo=600"
else
  OPTIONS="vers=$VERS,proto=tcp,port=$PORT,hard,timeo=600"
fi
echo "export: $(tr '\n' ' ' < /tmp/export.txt)"; echo "options: $OPTIONS"
mkdir -p /mnt/v; trap "umount -f -l /mnt/v 2>/dev/null" EXIT
mount -t nfs -o "$OPTIONS" "127.0.0.1:$EXPORT" /mnt/v || { echo MOUNT-FAILED; cat /tmp/anchor.err | tail -5; exit 1; }
daemon_pid() { awk -v a="$ANCHOR" '/^PPid:/ && $2 == a {split(FILENAME, p, "/"); print p[3]; exit}' /proc/[0-9]*/status 2>/dev/null; }
python3 - /mnt/v /tmp/record "$DURATION" <<'PY' &
import hashlib, os, random, sys, time
root, record, duration = sys.argv[1], sys.argv[2], float(sys.argv[3])
rng = random.Random(7)
live, deleted, errors = [], set(), []
out = open(record, "w")
started, n, step = time.time(), 0, 0
while time.time() - started < duration:
    name = f"f{n}"; n += 1
    data = rng.randbytes(rng.randrange(100, 300 * 1024))
    try:
        fd = os.open(os.path.join(root, name), os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
        view = memoryview(data)
        while view:
            view = view[os.write(fd, view):]
        os.fsync(fd)
        os.close(fd)
    except OSError as e:
        errors.append(f"{name}: {e}"); continue
    out.write(f"{name} {len(data)} {hashlib.sha256(data).hexdigest()}\n"); out.flush()
    live.append(name)
    step += 1
    if len(live) > 2000 and step % 2 == 0:
        old = live.pop(0)
        try:
            os.unlink(os.path.join(root, old)); deleted.add(old); out.write(f"- {old}\n"); out.flush()
        except OSError as e:
            errors.append(f"unlink {old}: {e}")
print(f"writer: {n} files written, {len(errors)} errors {errors[:5]}", flush=True)
PY
WRITER=$!
rng_sleep() { python3 -c "import random;print(round(random.uniform(0.4,1.6),3))"; }
for k in $(seq 1 "$KILLS"); do
  sleep "$(rng_sleep)"; P=$(daemon_pid); [ -n "$P" ] && kill -9 "$P" && echo "kill $k: daemon $P"
done
wait $WRITER
sync; umount /mnt/v; mount -t nfs -o "$OPTIONS" "127.0.0.1:$EXPORT" /mnt/v || { echo REMOUNT-FAILED; exit 1; }
python3 - /mnt/v /tmp/record <<'PY'
import hashlib, os, sys
root, record = sys.argv[1], sys.argv[2]
files, deleted = {}, set()
for line in open(record):
    parts = line.split()
    if parts[0] == "-": deleted.add(parts[1]); files.pop(parts[1], None)
    else: files[parts[0]] = (int(parts[1]), parts[2])
intact = corrupt = empty = resurrected = missing = 0; bad = []
for name, (size, digest) in files.items():
    p = os.path.join(root, name)
    try: data = open(p, "rb").read()
    except FileNotFoundError: missing += 1; bad.append(f"{name} missing"); continue
    if hashlib.sha256(data).hexdigest() == digest: intact += 1
    else:
        corrupt += 1; empty += (len(data) == 0); bad.append(f"{name} {len(data)} of {size} bytes")
for name in deleted:
    if os.path.exists(os.path.join(root, name)): resurrected += 1
print(f"checked {len(files)}: intact {intact} corrupt {corrupt} (empty {empty}) missing {missing} deleted {len(deleted)} resurrected {resurrected}")
for b in bad[:20]: print("  BAD", b)
PY
grep -c 'restarted' /tmp/anchor.err | sed 's/^/anchor restarts logged: /'
