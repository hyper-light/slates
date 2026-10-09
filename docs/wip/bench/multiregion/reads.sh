#!/bin/sh
# Cross-region read timing on the two-network topology (`run.sh`; condition 10's remote pull). Brings the topology
# up afresh, waits until a0's detector holds all five peers (cross-region membership formed, bounded at 120 s),
# bootstraps the root on a0 and region 1 on b0, creates a volume on a1 and writes PAYLOAD into it, then times ROUNDS
# whole reads of the file from a0 (same region as the owner) and from b0, b1 and b2 (the other region). Each read
# prints `node:exit/seconds/sum`, where `exit` is the read's own exit status and `sum` is the BSD `sum` checksum of the
# bytes read (compare with `sum PAYLOAD`); every refused read's error is printed after the rounds. It ends with the owner's record sessions to each reader (window, round
# trip, loss) and b0's forwarding and read-ahead counters.
#
#   sh docs/wip/bench/multiregion/reads.sh SCRATCH_DIR PAYLOAD [ROUNDS]
#   JITTER=0ms IMAGE=slates:mr sh docs/wip/bench/multiregion/reads.sh SCRATCH_DIR PAYLOAD 3
#   CONCURRENT=1 ... starts the three region-1 readers together each round (contested), instead of one at a time
#
# The shaping variables (DELAY, JITTER, LOSS, IMAGE) pass through to `run.sh`. Nothing touches the repository's
# tree; the payload is copied into the scratch directory, which `run.sh` mounts for the writer.
set -u
DIR=${1:?scratch directory}; PAYLOAD=${2:?payload file}; ROUNDS=${3:-3}
HERE=$(dirname "$0")
sh "$HERE/run.sh" "$DIR" down > /dev/null 2>&1
sh "$HERE/run.sh" "$DIR" up > "$DIR/up.txt" 2>&1 || { echo "up failed (see $DIR/up.txt)"; exit 1; }
formed=no
for attempt in $(seq 1 24); do
  peers=$(docker exec mr-a0 /slates status --json 2>/dev/null |
    python3 -c 'import json,sys;print(len(json.load(sys.stdin)["fleet"]["detector"]))' 2>/dev/null)
  [ "$peers" = "5" ] && { formed="yes after $((attempt * 5)) s"; break; }
  python3 -c 'import time;time.sleep(5)'
done
echo "formed: $formed"
[ "$formed" = "no" ] && exit 2
cp "$PAYLOAD" "$DIR/payload.bin"
docker exec mr-a0 /slates bootstrap root --json > /dev/null || exit 1
for attempt in $(seq 1 60); do docker exec mr-b0 /slates bootstrap region --json > /dev/null 2>&1 && break; done
for attempt in $(seq 1 60); do
  VOLUME=$(docker exec mr-a1 /slates volume create reads --dynamic 256MiB --json 2>"$DIR/create.err" |
    python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])' 2>/dev/null) && [ -n "$VOLUME" ] && break
  python3 -c 'import time;time.sleep(1)'
done
[ -n "$VOLUME" ] || { echo "create refused 60 times; the last: $(head -c 300 "$DIR/create.err")"; exit 1; }
ATTACHMENT=$(docker exec mr-a1 /slates attach "$VOLUME" --write --json |
  python3 -c 'import sys,json; print(json.load(sys.stdin)["attachment"])')
docker exec -i mr-a1 /slates write "$VOLUME" "$ATTACHMENT" /payload.bin < "$DIR/payload.bin" > /dev/null
echo "volume $VOLUME, payload sum $(sum "$DIR/payload.bin" | cut -d' ' -f1)"
now() { python3 -c 'import time;print(time.time())'; }
errors=""
if [ "${CONCURRENT:-0}" = 1 ]; then
  # Contested: each round starts the three region-1 readers at once, all pulling the file across the crossing together.
  for round in $(seq 1 "$ROUNDS"); do
    began=$(now)
    for node in b0 b1 b2; do
      ( timeout 600 docker exec "mr-$node" /slates read "$VOLUME" /payload.bin > "$DIR/read-$node.out" 2> "$DIR/read-$node.err"
        echo $? > "$DIR/read-$node.status"
        python3 -c 'import time;print(time.time())' > "$DIR/read-$node.end" ) &
    done
    wait
    line=""
    for node in b0 b1 b2; do
      status=$(cat "$DIR/read-$node.status")
      checksum=$(sum < "$DIR/read-$node.out" | cut -d' ' -f1)
      line="$line $node:$status/$(python3 -c "print(round($(cat "$DIR/read-$node.end") - $began, 2))")s/$checksum"
      [ "$status" -ne 0 ] && errors="$errors
  round $round $node: $(head -c 300 "$DIR/read-$node.err")"
    done
    echo "concurrent$line"
  done
fi
# One reader at a time (skipped in the contested mode; BSD `seq 1 0` counts down, so a zero count is not enough).
[ "${CONCURRENT:-0}" = 1 ] || for round in $(seq 1 "$ROUNDS"); do
  line=""
  for node in a0 b0 b1 b2; do
    began=$(now)
    # The read's own exit status and error, kept apart from the checksum: until 2026-10-09 `$?` was the status of the
    # pipeline's last command (`cut`), so a refused read printed exit 0 and its reason was discarded.
    timeout 600 docker exec "mr-$node" /slates read "$VOLUME" /payload.bin > "$DIR/read.out" 2> "$DIR/read.err"
    status=$?
    checksum=$(sum < "$DIR/read.out" | cut -d' ' -f1)
    line="$line $node:$status/$(python3 -c "print(round($(now) - $began, 2))")s/$checksum"
    [ "$status" -ne 0 ] && errors="$errors
  round $round $node: $(head -c 300 "$DIR/read.err")"
  done
  echo "$line"
done
[ -n "$errors" ] && echo "refused reads:$errors"
# The owner's record session to each reader (a1 sends the bytes): its window, round trip and loss when last seen.
docker exec mr-a1 /slates status --json 2>/dev/null | python3 -c '
import json, sys
fleet = json.load(sys.stdin)["fleet"]
for kind in ("sessions", "served_sessions"):
  for session in fleet.get(kind, []):
    print(kind, end=" ")
    print({key: session.get(key) for key in ("peer", "lent", "congestion_window", "smoothed_rtt_ns", "pto_ns", "spurious_losses", "persistent_collapses", "bytes_consumed")})'
for node in b0 b1 b2; do
  printf '%s ' "$node"
docker exec "mr-$node" /slates status --json 2>/dev/null | python3 -c '
import json, sys, collections
counts = collections.Counter()
for shard in json.load(sys.stdin)["shards"]:
    for refusal in shard["refusals"]:
        counts[refusal["kind"]] += refusal["count"]
print({kind: n for kind, n in counts.items() if "read_ahead" in kind or "owner_location" in kind or "forward" in kind})'
done
uptime
