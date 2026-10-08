#!/bin/sh
# Cross-region read timing on the two-network topology (`run.sh`; condition 10's remote pull). Brings the topology
# up afresh, waits until a0's detector holds all five peers (cross-region membership formed, bounded at 120 s),
# bootstraps the root on a0 and region 1 on b0, creates a volume on a1 and writes PAYLOAD into it, then times ROUNDS
# whole reads of the file from a0 (same region as the owner) and from b0, b1 and b2 (the other region). Each read
# prints `node:exit/seconds/sum`, where `sum` is the BSD `sum` checksum of the bytes read (compare with
# `sum PAYLOAD`); 0 means the read refused. It ends with the owner's record sessions to each reader (window, round
# trip, loss) and b0's forwarding and read-ahead counters.
#
#   sh docs/wip/bench/multiregion/reads.sh SCRATCH_DIR PAYLOAD [ROUNDS]
#   JITTER=0ms IMAGE=slates:mr sh docs/wip/bench/multiregion/reads.sh SCRATCH_DIR PAYLOAD 3
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
  VOLUME=$(docker exec mr-a1 /slates volume create reads --dynamic 256MiB --json 2>/dev/null |
    python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])' 2>/dev/null) && [ -n "$VOLUME" ] && break
done
ATTACHMENT=$(docker exec mr-a1 /slates attach "$VOLUME" --write --json |
  python3 -c 'import sys,json; print(json.load(sys.stdin)["attachment"])')
docker exec -i mr-a1 /slates write "$VOLUME" "$ATTACHMENT" /payload.bin < "$DIR/payload.bin" > /dev/null
echo "volume $VOLUME, payload sum $(sum "$DIR/payload.bin" | cut -d' ' -f1)"
now() { python3 -c 'import time;print(time.time())'; }
for round in $(seq 1 "$ROUNDS"); do
  line=""
  for node in a0 b0 b1 b2; do
    began=$(now)
    checksum=$(timeout 600 docker exec "mr-$node" /slates read "$VOLUME" /payload.bin 2>/dev/null | sum | cut -d' ' -f1)
    status=$?
    line="$line $node:$status/$(python3 -c "print(round($(now) - $began, 2))")s/$checksum"
  done
  echo "$line"
done
# The owner's record session to each reader (a1 sends the bytes): its window, round trip and loss when last seen.
docker exec mr-a1 /slates status --json 2>/dev/null | python3 -c '
import json, sys
fleet = json.load(sys.stdin)["fleet"]
for kind in ("sessions", "served_sessions"):
  for session in fleet.get(kind, []):
    print(kind, end=" ")
    print({key: session.get(key) for key in ("peer", "lent", "congestion_window", "smoothed_rtt_ns", "pto_ns", "spurious_losses", "persistent_collapses", "bytes_consumed")})'
docker exec mr-b0 /slates status --json 2>/dev/null | python3 -c '
import json, sys, collections
counts = collections.Counter()
for shard in json.load(sys.stdin)["shards"]:
    for refusal in shard["refusals"]:
        counts[refusal["kind"]] += refusal["count"]
print({kind: n for kind, n in counts.items() if "read_ahead" in kind or "owner_location" in kind or "forward" in kind})'
uptime
