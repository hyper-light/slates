#!/usr/bin/env bash
# Goal 93 perf (main's order of 2026-11-21 03:36): where slates spends the first push of a large Mercurial history,
# by phase. One run: a fresh `slates anchor --vcs-http 127.0.0.1:0 --vcs-store OUT/store` from the release build, a
# Mercurial repository and a token as `hg_scale.sh` makes them, then SOURCE pushed whole in one push, the repository's
# first, while the daemon is sampled (macOS `sample`, 10 ms) in consecutive windows of WINDOW seconds and its resident
# set and CPU time read every second. Each window's frames are summed by function (the most samples any one stack
# position gave it, so a frame is never counted twice within a window), which reads the push's phases in time order:
# intake decode, index inserts, the store's landing, the register's commit. The client's side is kept too: its debug
# lines stamped with the seconds since the push began (`push.timeline`: discovery, the bundle's parts as it writes them,
# the unbundle request and its answer) and its own CPU (`/usr/bin/time -l`), so a phase the daemon waits through is
# named as well.
#
# With STEP=stream the push runs unmeasured first, and the step measured is a stream clone of what it landed
# (`hg clone -U --stream`): the same timeline, samples and resident set, and the bytes the client received.
#
# ROUNDS=N (stream only) times N stream clones first, unsampled, each's wall time and the bytes its client received,
# the first checked by `hg verify`: the A/B's slates side, all N shown.
#
# Usage: [SAMPLE=0] [STEP=push|stream] [ROUNDS=N] hg_push_profile.sh SLATES HG SOURCE OUT [WINDOW]
#   SLATES  the release `slates` binary;  HG  stock hg 7.2;  SOURCE  the history pushed;  OUT  a directory made here
#   WINDOW  seconds a sample window covers (default 20);  SAMPLE=0  no sampling (the daemon at full speed)
set -uo pipefail
SLATES=$1 HG_BIN=$2 SOURCE=$(cd "$3" && pwd) OUT=$4 WINDOW=${5:-20}
mkdir -p "$OUT" && OUT=$(cd "$OUT" && pwd) || exit 1
H=(env "HOME=$OUT" HGRCPATH= HGPLAIN=1 LC_ALL=C "$HG_BIN" --noninteractive)
now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
first() { sed -n "$2" "$1" | head -1; }
instance="w-perf-hg-push-$$" log="$OUT/anchor.log"
rm -rf "$OUT/store" && mkdir -p "$OUT/store"
"$SLATES" --instance "$instance" anchor --vcs-http 127.0.0.1:0 --vcs-store "$OUT/store" > /dev/null 2> "$log" &
ANCHOR=$!
trap 'kill -9 $ANCHOR 2> /dev/null; pkill -9 -f -- "$instance" 2> /dev/null; rm -rf "$OUT/store"' EXIT
for _ in $(seq 1 1200); do
  grep -q "^slates daemon: instance $instance serving on" "$log" && grep -q '^slates anchor: issuer surface: export ' "$log" && break
  sleep 0.1
done
daemon() { grep -oE '(daemon pid|restarted as pid) [0-9]+' "$log" | tail -1 | awk '{print $NF}'; }
port=$(first "$log" 's/^slates daemon: the version-control HTTP edge serves cleartext on 127\.0\.0\.1:\([0-9]*\).*/\1/p')
exports=$(first "$log" 's/^slates anchor: issuer surface: export //p')
streak=0
for _ in $(seq 1 1200); do
  if "$SLATES" --instance "$instance" volume list > /dev/null 2>&1; then streak=$((streak + 1)); else streak=0; fi
  [ "$streak" -ge 10 ] && break
  sleep 0.1
done
"$SLATES" --instance "$instance" bootstrap root > /dev/null || exit 1
"$SLATES" --instance "$instance" repository create project --tool hg > /dev/null || exit 1
# shellcheck disable=SC2086 # the surface's NAME=VALUE pairs are words
token=$(env $exports "$SLATES" --instance "$instance" credential token | sed -n 's/^token: //p')
URL="http://127.0.0.1:$port/project"
AUTH=(--config "auth.slates.prefix=http://127.0.0.1:$port/" --config auth.slates.username=slates
  --config "auth.slates.password=$token")
# hg run once before the clock starts: a wrapper that builds its environment under HOME on first use (`uvx`) took 30 s
# of the first push it ran in, the daemon idle throughout (r18, 2026-11-21: "pushing to" at 29.93 s).
"${H[@]}" --version > /dev/null
STEP=${STEP:-push}
MEASURED=(-R "$SOURCE" push "$URL")
if [ "$STEP" = stream ]; then
  t=$(now)
  "${H[@]}" "${AUTH[@]}" -R "$SOURCE" push -q "$URL" || { echo "the unmeasured push failed"; exit 1; }
  echo "push (unmeasured) $(perl -e 'printf "%.1f", $ARGV[1] - $ARGV[0]' "$t" "$(now)") s"
  MEASURED=(clone -U --stream "$URL" "$OUT/stream-clone")
  for round in $(seq 1 "${ROUNDS:-0}"); do
    rm -rf "$OUT/round-clone"
    t=$(now)
    said=$("${H[@]}" "${AUTH[@]}" clone -U --stream --debug "$URL" "$OUT/round-clone" 2>&1 | grep -E "^\(sent |files to transfer|abort")
    echo "slates stream $round $(perl -e 'printf "%.3f", $ARGV[1] - $ARGV[0]' "$t" "$(now)") s; $(echo "$said" | tr '\n' ' ')"
    [ "$round" = 1 ] && echo "slates stream verify: $("${H[@]}" -R "$OUT/round-clone" verify 2>&1 | grep -E 'checked|warning|error|integrity' | tr '\n' ' ')"
  done
  rm -rf "$OUT/round-clone"
fi
D=$(daemon)
echo "daemon $D; step $STEP; load $(sysctl -n vm.loadavg)"
start=$(now)
# The step, its debug lines stamped as they arrive; time(1)'s report on the client's CPU after them.
( /usr/bin/time -l env PYTHONUNBUFFERED=1 "${H[@]}" "${AUTH[@]}" "${MEASURED[@]}" --debug --config devel.debug.peer-request=yes 2>&1 \
  | perl -MTime::HiRes=time -ne 'BEGIN { $s = shift } printf "%8.3f %s", time - $s, $_' "$start" > "$OUT/push.timeline" ) &
PUSH=$!
# Resident set (MiB) and CPU time each second, beside the wall clock since the push began.
(while kill -0 $PUSH 2> /dev/null; do
  echo "$(perl -e 'printf "%.0f", $ARGV[1] - $ARGV[0]' "$start" "$(now)") $(ps -o rss=,cputime= -p "$D" | awk '{printf "%.0f %s", $1 / 1024, $2}')"
  sleep 1
done) > "$OUT/rss.txt" &
window=0
while [ "${SAMPLE:-1}" != 0 ] && kill -0 $PUSH 2> /dev/null; do
  window=$((window + 1))
  sample "$D" "$WINDOW" 10 -mayDie -file "$OUT/window-$(printf %02d $window).sample" > /dev/null 2>&1
done
wait $PUSH
code=$?
echo "$STEP $(perl -e 'printf "%.3f", $ARGV[1] - $ARGV[0]' "$start" "$(now)") s exit $code; daemon $(daemon) (was $D); restarts $(grep -c 'restarted as pid' "$log")"
echo "client timeline (phase marks):"
grep -E "^ *[0-9.]+ (pushing to|streaming all|[0-9]+ files to transfer|transferred|\(sent |sending stream_out|query 1|searching|sending (capabilities|batch|unbundle|heads|known)|devel-peer-request: +finished|bundle2-output-(bundle|part)|bundle2-input-(bundle|part): total|remote: |added |[0-9]+ (changesets|files)|abort)" "$OUT/push.timeline" | head -40
echo "client CPU:"; grep -E "real|user|sys|maximum resident" "$OUT/push.timeline" | tail -4
echo "rss MiB and CPU time by second (every 10th):"; awk 'NR % 10 == 1' "$OUT/rss.txt"; tail -1 "$OUT/rss.txt"
python3 - "$OUT" "$WINDOW" << 'PY'
# Each window's call tree, per thread: a frame's busy samples are its samples less those whose stack ends in a kernel
# wait (a parked shard, the store owner idle, the watchdog's sleep), each frame counted once per stack however often it
# recurs. Rust's v0 symbols read back as their path's names (`slates_hg::push::intake::step`), generics dropped.
import glob, re, sys
out, window = sys.argv[1], int(sys.argv[2])
WAITS = {"semaphore_wait_trap", "semaphore_timedwait_trap", "__psynch_cvwait", "kevent", "kevent64", "kevent_id", "__semwait_signal",
         "mach_msg2_trap", "mach_msg_trap", "__ulock_wait", "__ulock_wait2", "__workq_kernreturn", "__select",
         "__psynch_mutexwait", "poll", "__sigsuspend", "__wait4", "nanosleep"}
def demangle(name):
    # v0: identifiers are a decimal length, an optional `_`, then the bytes; `s<base62>_` disambiguators and
    # `B<base62>_` back-references are skipped, scanning left to right so an identifier's own letters are never read.
    if not name.startswith("_R"):
        return name
    parts, at = [], 2
    while at < len(name):
        char = name[at]
        if char.isdigit():
            end = at
            while end < len(name) and name[end].isdigit():
                end += 1
            length = int(name[at:end])
            if end < len(name) and name[end] == "_":
                end += 1
            ident = name[end:end + length]
            if length and re.match(r'^[A-Za-z_][A-Za-z0-9_]*$', ident):
                parts.append(ident)
            at = end + length
        elif char in "sB" and re.match(r'^[0-9A-Za-z]*_', name[at + 1:]):
            at = name.index("_", at + 1) + 1
        else:
            at += 1
    keep = [part for part in parts if part not in ("core", "std", "alloc")] or parts
    return "::".join(keep[-6:]) if keep else name
row = re.compile(r'^([\s+!:|]*)(\d+)\s+(.*)$')
for path in sorted(glob.glob(f"{out}/window-*.sample")):
    lines = open(path, errors="replace").read().split("\n")
    try:
        lines = lines[lines.index("Call graph:") + 1:]
    except ValueError:
        continue
    nodes = []
    for text in lines:
        if text.startswith("Total number in stack") or text.startswith("Sort by top of stack") or not text.strip():
            if nodes:
                break
            continue
        found = row.match(text)
        if not found:
            continue
        depth, count, rest = len(found.group(1)), int(found.group(2)), found.group(3)
        name = re.sub(r'\s+\(in [^)]*\).*$', '', rest).strip()
        name = re.sub(r'\s+\+ \d+.*$', '', name)
        nodes.append([depth, count, name, []])
    # Build the tree by depth; a thread is a node whose name begins "Thread_".
    roots, stack = [], []
    for node in nodes:
        while stack and stack[-1][0] >= node[0]:
            stack.pop()
        (stack[-1][3] if stack else roots).append(node)
        stack.append(node)
    def idle(node):
        if not node[3]:
            return node[1] if node[2] in WAITS else 0
        below = sum(child[1] for child in node[3])
        return sum(idle(child) for child in node[3]) + (0 if below else 0)
    busy_by_name, threads = {}, []
    def walk(node, seen):
        busy = node[1] - idle(node)
        name = demangle(node[2])
        if busy > 0 and name not in seen:
            busy_by_name[name] = busy_by_name.get(name, 0) + busy
        for child in node[3]:
            walk(child, seen | {name})
    for thread in roots:
        busy = thread[1] - idle(thread)
        threads.append((busy, thread[1], thread[2][:70]))
        walk(thread, frozenset())
    print(f"== {path.rsplit('/', 1)[1]} ({window} s at 10 ms): busy samples by thread")
    for busy, total, name in sorted(threads, reverse=True)[:5]:
        print(f"   {busy:6d} of {total:6d}  {name}")
    print("   busy samples by frame (each frame once a stack):")
    for name, count in sorted(busy_by_name.items(), key=lambda item: -item[1])[:60]:
        if not name.startswith("Thread_"):
            print(f"   {count:6d} {name[:140]}")
PY
