#!/bin/bash
# A/B of two slates binaries through `slates mount` (Apple's NFSv3 client), no kills: kill_fsync_macos.sh's writer for
# DURATION seconds (files written whole, fsynced, closed), then one file of BIG_MIB MiB written in 1 MiB writes and
# fsynced. Each round runs both binaries, the order alternating, each on a fresh anchor and volume. Mounting without
# kills is safe on any Mac (macos-nfs-kill-test-panics-the-mac). Made for A-358's cost: a publication per WRITE.
#   bash ab_write_macos.sh <slates A> <slates B> [ROUNDS=3 DURATION=20 BIG_MIB=512]
set -u
A=$1; B=$2; ROUNDS=${ROUNDS:-3}; DURATION=${DURATION:-20}; BIG_MIB=${BIG_MIB:-512}
WORK=$HOME/.cache/g079/ab-$$; mkdir -p "$WORK"
PY=/usr/bin/python3
one() {
  local label=$1 bin=$2 dir="$WORK/$1-$3" inst="ab-$$-$1-$3"
  mkdir -p "$dir/mnt"
  "$bin" --instance "$inst" anchor --quick --shards 4 >"$dir/anchor.out" 2>"$dir/anchor.err" &
  local anchor=$! n=0
  while [ "$n" -lt 300 ]; do "$bin" --instance "$inst" volume list >/dev/null 2>&1 && break; sleep 0.1; n=$((n + 1)); done
  "$bin" --instance "$inst" bootstrap root >/dev/null 2>&1
  local id; id=$("$bin" --instance "$inst" volume create ab --dynamic 4GiB 2>&1 | awk '/^id/{print $2}')
  "$bin" --instance "$inst" mount "$id" "$dir/mnt" >/dev/null || { echo "$label: MOUNT-FAILED"; kill -TERM $anchor; return; }
  "$PY" - "$dir/mnt" "$DURATION" "$BIG_MIB" "$label" <<'PY'
import os, random, sys, time
root, duration, big, label = sys.argv[1], float(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
rng = random.Random(7)
started, n, live, step, latencies = time.time(), 0, [], 0, []
while time.time() - started < duration:
    name = f"f{n}"; n += 1
    data = rng.randbytes(rng.randrange(100, 300 * 1024))
    t0 = time.perf_counter()
    fd = os.open(os.path.join(root, name), os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
    view = memoryview(data)
    while view:
        view = view[os.write(fd, view):]
    os.fsync(fd); os.close(fd)
    latencies.append(time.perf_counter() - t0)
    live.append(name); step += 1
    if len(live) > 2000 and step % 2 == 0:
        os.unlink(os.path.join(root, live.pop(0)))
latencies.sort()
q = lambda p: latencies[min(len(latencies) - 1, int(p * len(latencies)))] * 1e3
chunk = os.urandom(1 << 20)
t0 = time.perf_counter()
fd = os.open(os.path.join(root, "big"), os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
for _ in range(big):
    view = memoryview(chunk)
    while view:
        view = view[os.write(fd, view):]
t1 = time.perf_counter()
os.fsync(fd); os.close(fd)
t2 = time.perf_counter()
print(f"{label}: files {n} in {duration:.0f} s ({n / duration:.0f}/s), file p50 {q(0.5):.2f} ms p99 {q(0.99):.2f} ms; "
      f"{big} MiB: writes {t1 - t0:.2f} s + fsync {t2 - t1:.2f} s = {big / (t2 - t0):.0f} MiB/s", flush=True)
PY
  "$bin" --instance "$inst" status 2>/dev/null | grep -E "nfs_calls" | sed "s/^/  $label /"
  "$bin" --instance "$inst" unmount "$dir/mnt" >/dev/null 2>&1 || /sbin/umount "$dir/mnt" || /sbin/umount -f "$dir/mnt"
  kill -TERM $anchor; wait $anchor 2>/dev/null
}
echo "ab_write: $(sw_vers -productVersion) $(sysctl -n machdep.cpu.brand_string); load $(sysctl -n vm.loadavg)"
r=0
while [ "$r" -lt "$ROUNDS" ]; do
  r=$((r + 1))
  if [ $((r % 2)) -eq 1 ]; then one A "$A" "$r"; one B "$B" "$r"; else one B "$B" "$r"; one A "$A" "$r"; fi
  echo "  load after round $r: $(sysctl -n vm.loadavg)"
done
rm -rf "$WORK"
