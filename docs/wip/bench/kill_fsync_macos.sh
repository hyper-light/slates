#!/bin/bash
# GAPS 2026-10-06 "two fsync-acknowledged files came back empty after daemon kills on the macOS NFS mount", asked of the
# macOS kernel client at HEAD, as kill_fsync.sh asks it of the Linux client. The writer is that run's: files of 100 B to
# 300 KB written whole and fsynced, each hashed outside the volume only once its fsync and close returned, and past 2,000
# files an old one deleted every other step. It writes for DURATION seconds through `slates mount` (Apple's NFSv3 kernel
# client, hard,intr), while the daemon is SIGKILLed KILLS times at random LOW-HIGH s intervals and the anchor restarts it.
# Afterwards the volume is unmounted while a daemon lives and mounted again (a cold client cache), and every recorded file
# must read back with its hash, every deleted name absent. ROUNDS fresh anchors run in turn.
#
# Before A-113 this experiment panicked a Mac twice (macos-nfs-kill-test-panics-the-mac). Run it only on a machine whose
# panic you can afford, with SINK=host:port to stream the log off it as it runs (`log_sink.py PORT FILE` there). Every
# line of the log is written to WORK/run.log and fsynced, written to standard output unbuffered, and sent to the sink,
# each before the next line: a panic loses at most the line in hand (a block-buffered filter or a `tail -F | nc` once
# lost a whole run's lines, 2026-11-18).
#   SLATES=<release slates> [ROUNDS=1 KILLS=15 DURATION=30 LOW=0.4 HIGH=1.6 SINK=host:port WORK=dir] bash kill_fsync_macos.sh
#
# The script never stops a daemon under a live mount except by the kills: its teardown stops the writer, unmounts while
# a daemon serves, and only then stops the anchor. HANG bounds how long past DURATION the writer may take before the
# round is called stuck: a kill-to-answer is about 55 ms since A-114, so a writer still going HANG seconds late is
# waiting on a daemon that never answers, not on a slow restart.
set -u
SLATES=${SLATES:?SLATES must name the slates release binary}
ROUNDS=${ROUNDS:-1}; KILLS=${KILLS:-15}; DURATION=${DURATION:-30}; LOW=${LOW:-0.4}; HIGH=${HIGH:-1.6}
HANG=${HANG:-120}; PY=${PY:-/usr/bin/python3}
WORK=${WORK:-$HOME/.cache/kill-fsync-macos/$$}
mkdir -p "$WORK"
LOG=$WORK/run.log
: > "$LOG"
# The logger: each line appended to the log and fsynced, written on, and sent to the sink (reconnecting at most once a
# second while the sink is away), in that order, before the next line is read.
exec > >("$PY" -u -c '
import os, socket, sys, time
path, sink = sys.argv[1], sys.argv[2]
log = open(path, "a")
conn, tried = None, None
for line in iter(sys.stdin.readline, ""):
    log.write(line); log.flush(); os.fsync(log.fileno())
    sys.stdout.write(line); sys.stdout.flush()
    if sink and conn is None and (tried is None or time.monotonic() - tried >= 1.0):
        tried = time.monotonic()
        host, port = sink.rsplit(":", 1)
        try:
            conn = socket.create_connection((host, int(port)), timeout=0.5)
        except OSError:
            conn = None
    if conn is not None:
        try:
            conn.sendall(line.encode("utf-8", "replace"))
        except OSError:
            conn = None
' "$LOG" "${SINK:-}") 2>&1
now() { perl -MTime::HiRes=time -e 'printf "%.3f", time'; }
say() { echo "[$(date -u +%H:%M:%S)] $*"; }

ANCHOR=""; INSTANCE=""; MNT=""; MOUNTED=0; WRITER=""

daemon_pid() { [ -n "$ANCHOR" ] && pgrep -P "$ANCHOR" | head -1; }

await_daemon() {
  local waited=0
  while [ "$waited" -lt 300 ]; do
    "$SLATES" --instance "$INSTANCE" volume list >/dev/null 2>&1 && return 0
    sleep 0.1; waited=$((waited + 1))
  done
  return 1
}

unmount_live() {
  [ "$MOUNTED" = 1 ] || return 0
  if ! await_daemon; then say "UNMOUNT: no daemon answers; unmounting anyway"; fi
  "$SLATES" --instance "$INSTANCE" unmount "$MNT" >/dev/null 2>&1 \
    || /sbin/umount "$MNT" 2>/dev/null || /sbin/umount -f "$MNT"
  if mount | grep -q " on $MNT "; then say "UNMOUNT-FAILED $MNT"; return 1; fi
  MOUNTED=0
}

stop_anchor() {
  [ -n "$ANCHOR" ] || return 0
  kill -TERM "$ANCHOR" 2>/dev/null
  local waited=0
  while kill -0 "$ANCHOR" 2>/dev/null && [ "$waited" -lt 150 ]; do sleep 0.1; waited=$((waited + 1)); done
  kill -0 "$ANCHOR" 2>/dev/null && { say "anchor $ANCHOR did not stop in order; SIGKILL"; kill -KILL "$ANCHOR"; }
  wait "$ANCHOR" 2>/dev/null
  ANCHOR=""
}

teardown() {
  if [ -n "$WRITER" ] && kill -0 "$WRITER" 2>/dev/null; then
    kill -TERM "$WRITER" 2>/dev/null; sleep 2; kill -KILL "$WRITER" 2>/dev/null
  fi
  WRITER=""
  if unmount_live; then stop_anchor; else say "LEFT RUNNING: anchor $ANCHOR serving $MNT (never stopped under a mount)"; fi
}

finish() {
  teardown
}
trap finish EXIT
trap 'exit 130' INT TERM HUP

# The daemon's NFS refusal counters (`nfs.writeback_unknown` and the rest) and its NFS calls by procedure, per shard, as
# `slates status` prints them: taken just before each kill, so each daemon's totals are on record.
counters() {
  "$SLATES" --instance "$INSTANCE" status 2>/dev/null | grep -E "refused nfs\.|nfs_calls" | sed 's/^shard //' | tr '\n' ';'
}

run_round() {
  local round=$1 dir="$WORK/r$1"
  INSTANCE="kfm-$$-$round"; MNT="$dir/mnt"
  mkdir -p "$MNT"
  say "round $round: instance $INSTANCE, work $dir"
  "$SLATES" --instance "$INSTANCE" anchor --quick --shards 4 >"$dir/anchor.out" 2>"$dir/anchor.err" &
  ANCHOR=$!
  await_daemon || { say "round $round: NO-DAEMON"; tail -5 "$dir/anchor.err"; return 1; }
  "$SLATES" --instance "$INSTANCE" bootstrap root >/dev/null 2>&1
  local id
  id=$("$SLATES" --instance "$INSTANCE" volume create kf --dynamic 2GiB 2>&1 | awk '/^id/{print $2}')
  [ -n "$id" ] || { say "round $round: VOLUME-CREATE-FAILED"; return 1; }
  "$SLATES" --instance "$INSTANCE" mount "$id" "$MNT" || { say "round $round: MOUNT-FAILED"; return 1; }
  MOUNTED=1
  say "mounted $id at $MNT; daemon $(daemon_pid); anchor children: $(pgrep -P "$ANCHOR" | tr '\n' ' ')"
  nfsstat -m 2>/dev/null | grep -A14 "$MNT" | sed 's/^/  /'
  "$PY" - "$MNT" "$dir/record" "$DURATION" <<'PY' &
import hashlib, os, random, sys, time
root, record, duration = sys.argv[1], sys.argv[2], float(sys.argv[3])
rng = random.Random(7)
live, errors = [], []
out = open(record, "w")
started, n, step, slowest = time.time(), 0, 0, (0.0, "")
while time.time() - started < duration:
    name = f"f{n}"; n += 1
    data = rng.randbytes(rng.randrange(100, 300 * 1024))
    try:
        opened = time.time()
        fd = os.open(os.path.join(root, name), os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
        view = memoryview(data)
        while view:
            view = view[os.write(fd, view):]
        synced = time.time()
        os.fsync(fd)
        done = time.time()
        os.close(fd)
        closed = time.time()
    except OSError as e:
        errors.append(f"{name}: {e}"); continue
    if closed - opened > slowest[0]:
        slowest = (closed - opened, name)
    out.write(f"{name} {len(data)} {hashlib.sha256(data).hexdigest()} {opened:.3f} {synced:.3f} {done:.3f} {closed:.3f}\n")
    out.flush()
    live.append(name)
    step += 1
    if n % 2000 == 0:
        print(f"writer: {n} files, slowest {slowest[0] * 1000:.0f} ms ({slowest[1]})", flush=True)
    if len(live) > 2000 and step % 2 == 0:
        old = live.pop(0)
        try:
            os.unlink(os.path.join(root, old)); out.write(f"- {old}\n"); out.flush()
        except OSError as e:
            errors.append(f"unlink {old}: {e}")
print(f"writer: {n} files written, {len(errors)} errors {errors[:5]}, slowest {slowest[0] * 1000:.0f} ms ({slowest[1]})",
      flush=True)
PY
  WRITER=$!
  : > "$dir/kills"
  local k=0 pause victim
  # A counted loop, never `seq 1 "$KILLS"`: BSD seq counts down, so KILLS=0 gave the kills 1 and 0.
  while [ "$k" -lt "$KILLS" ]; do
    k=$((k + 1))
    pause=$(awk -v seed="$RANDOM$k" -v lo="$LOW" -v hi="$HIGH" 'BEGIN { srand(seed); printf "%.3f", lo + rand() * (hi - lo) }')
    sleep "$pause"
    kill -0 "$WRITER" 2>/dev/null || { say "writer ended before kill $k"; break; }
    victim=$(daemon_pid)
    [ -n "$victim" ] || { say "kill $k: no daemon to kill"; continue; }
    local before; before=$(counters)
    kill -9 "$victim" && { local at; at=$(now); echo "$at $victim" >> "$dir/kills"; say "kill $k: daemon $victim at $at; its counters: $before"; }
  done
  local waited=0 limit=$((DURATION + HANG))
  while kill -0 "$WRITER" 2>/dev/null && [ "$waited" -lt "$limit" ]; do sleep 1; waited=$((waited + 1)); done
  if kill -0 "$WRITER" 2>/dev/null; then
    say "round $round: WRITER-STUCK ${limit}s; daemon $(daemon_pid)"
    sample "$WRITER" 1 2>/dev/null | head -60
    "$SLATES" --instance "$INSTANCE" status 2>&1 | head -40
    return 1
  fi
  wait "$WRITER"; WRITER=""
  say "final daemon $(daemon_pid) counters: $(counters)"
  grep -c 'restarted as pid' "$dir/anchor.err" | sed 's/^/anchor restarts logged: /'
  grep -i -E 'crash loop|panic|refus' "$dir/anchor.err" | head -5
  unmount_live || return 1
  "$SLATES" --instance "$INSTANCE" mount "$id" "$MNT" || { say "round $round: REMOUNT-FAILED"; return 1; }
  MOUNTED=1
  "$PY" - "$MNT" "$dir/record" "$dir/kills" <<'PY'
import bisect, hashlib, os, sys
root, record, kills_path = sys.argv[1], sys.argv[2], sys.argv[3]
kills = sorted(float(line.split()[0]) for line in open(kills_path) if line.strip())
files, deleted = {}, set()
for line in open(record):
    parts = line.split()
    if parts[0] == "-":
        deleted.add(parts[1]); files.pop(parts[1], None)
    else:
        files[parts[0]] = (int(parts[1]), parts[2], [float(t) for t in parts[3:7]])
intact = corrupt = empty = resurrected = missing = 0; bad = []
spanning = sum(1 for (_, _, (opened, _, _, closed)) in files.values()
               if bisect.bisect_left(kills, opened) < bisect.bisect_right(kills, closed))
def near(times):
    opened, closed = times[0], times[3]
    before = [k for k in kills if k <= closed]
    last = before[-1] if before else None
    return f"opened {opened:.3f} closed {closed:.3f}; last kill before close: " + (
        f"{last:.3f} ({(closed - last) * 1000:.0f} ms before)" if last else "none")
for name, (size, digest, times) in files.items():
    p = os.path.join(root, name)
    try:
        data = open(p, "rb").read()
    except FileNotFoundError:
        missing += 1; bad.append(f"{name} missing; {near(times)}"); continue
    if hashlib.sha256(data).hexdigest() == digest:
        intact += 1
    else:
        corrupt += 1; empty += (len(data) == 0)
        bad.append(f"{name} {len(data)} of {size} bytes; {near(times)}")
for name in deleted:
    if os.path.exists(os.path.join(root, name)):
        resurrected += 1
print(f"checked {len(files)}: intact {intact} corrupt {corrupt} (empty {empty}) missing {missing} "
      f"deleted {len(deleted)} resurrected {resurrected}; {spanning} of them open across a kill; kills {len(kills)}",
      flush=True)
for b in bad[:40]:
    print("  BAD", b, flush=True)
PY
  say "round $round: done"
  teardown
}

say "kill_fsync_macos: $("$SLATES" version 2>&1); $(sw_vers -productVersion) $(sw_vers -buildVersion); $(uname -v | sed 's/.*root://')"
say "machine: $(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) cpus; load $(sysctl -n vm.loadavg)"
say "nfs kext: $(kmutil showloaded 2>/dev/null | grep -i filesystems.nfs | awk '{print $6, $7}')"
round=0
while [ "$round" -lt "$ROUNDS" ]; do
  round=$((round + 1))
  nfsstat -c 2>/dev/null | sed -n '/Rpc Info/,$p' | tr -s ' ' | sed "s/^/  client rpc before round $round: /"
  run_round "$round" || { say "round $round FAILED"; break; }
  nfsstat -c 2>/dev/null | sed -n '/Rpc Info/,$p' | tr -s ' ' | sed "s/^/  client rpc after round $round: /"
done
say "kill_fsync_macos: end"
