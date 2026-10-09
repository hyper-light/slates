#!/bin/sh
# Cross-region replication timing on the two-network topology (`run.sh`; condition 10): how long a sealed snapshot takes
# to be placed in the other region, its mirror (`docs/wip/mirroring.md`), under the router's shaping. Brings the
# topology up afresh, waits for cross-region membership, bootstraps the root on a0 and region 1 on b0, creates a volume
# on a1, then ROUNDS times writes PAYLOAD's bytes (each round a fresh copy of them, so every snapshot carries new
# content), snapshots, and polls `volume placed --mirror` until the snapshot is placed in region 1, printing
# `round:seconds/placed/mirror_age_ms`. A round that is not placed within LIMIT seconds prints `unplaced`.
#
#   sh docs/wip/bench/multiregion/mirror.sh SCRATCH_DIR PAYLOAD [ROUNDS]
#   DELAY=150ms JITTER=60ms LOSS=10% IMAGE=slates:mr sh docs/wip/bench/multiregion/mirror.sh SCRATCH_DIR PAYLOAD 3
#
# The shaping variables pass through to `run.sh`; nothing touches the repository's tree.
set -u
DIR=${1:?scratch directory}; PAYLOAD=${2:?payload file}; ROUNDS=${3:-3}; LIMIT=${LIMIT:-300}
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
docker exec mr-a0 /slates bootstrap root --json > /dev/null || exit 1
for attempt in $(seq 1 60); do docker exec mr-b0 /slates bootstrap region --json > /dev/null 2>&1 && break; done
VOLUME=""
for attempt in $(seq 1 60); do
  VOLUME=$(docker exec mr-a1 /slates volume create mirrored --dynamic 256MiB --json 2>"$DIR/create.err" |
    python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])' 2>/dev/null) && [ -n "$VOLUME" ] && break
  python3 -c 'import time;time.sleep(1)'
done
[ -n "$VOLUME" ] || { echo "create refused 60 times; the last: $(head -c 300 "$DIR/create.err")"; exit 1; }
echo "volume $VOLUME, payload $(wc -c < "$PAYLOAD" | tr -d ' ') bytes"
now() { python3 -c 'import time;print(time.time())'; }
for round in $(seq 1 "$ROUNDS"); do
  # A fresh copy of the payload each round (its first 16 bytes stamped with the round), so the snapshot is new content.
  { printf 'round %-10s\n' "$round"; tail -c +17 "$PAYLOAD"; } > "$DIR/payload.bin"
  # A snapshot closes the volume's write attachments (its barrier), so each round attaches again.
  ATTACHMENT=$(docker exec mr-a1 /slates attach "$VOLUME" --write --json |
    python3 -c 'import sys,json; print(json.load(sys.stdin)["attachment"])')
  docker exec -i mr-a1 /slates write "$VOLUME" "$ATTACHMENT" /payload.bin < "$DIR/payload.bin" > /dev/null || {
    echo "round $round: write refused"
    # Where the space went: each shard's reserve, committed bytes, held content and its refusals.
    docker exec mr-a1 /slates status --json 2>/dev/null | python3 -c '
import json, sys
for shard in json.load(sys.stdin)["shards"]:
    keep = {k: shard.get(k) for k in ("partition", "volumes", "reserve_bytes", "committed_bytes", "retained_bytes", "metadata_bytes", "committed_metadata")}
    refusals = {r["kind"]: r["count"] for r in shard.get("refusals", []) if "space" in r["kind"] or "budget" in r["kind"] or "hold" in r["kind"] or "content" in r["kind"]}
    print("  shard", keep, refusals)'
    continue; }
  SNAPSHOT=$(docker exec mr-a1 /slates volume snapshot "$VOLUME" --json 2>"$DIR/snap.err" |
    python3 -c 'import sys,json; print(json.load(sys.stdin)["snapshot"])' 2>/dev/null)
  [ -n "$SNAPSHOT" ] || { echo "round $round: snapshot refused: $(head -c 200 "$DIR/snap.err")"; continue; }
  began=$(now)
  # Home first: a mirror shipment starts only once the snapshot is placed in its own region (`fleet::mirror_due`).
  home=unplaced
  while [ "$(python3 -c "print(int($(now) - $began < $LIMIT))")" = 1 ]; do
    reply=$(docker exec mr-a1 /slates volume placed "$VOLUME" --snapshot "$SNAPSHOT" --json 2>/dev/null)
    case "$(echo "$reply" | python3 -c 'import sys,json; print(json.load(sys.stdin)["placed"])' 2>/dev/null)" in
      True) home="placed"; break ;;
    esac
    python3 -c 'import time;time.sleep(0.2)'
  done
  echo "round $round: home $(python3 -c "print(round($(now) - $began, 2))")s/$home"
  result=unplaced
  while [ "$(python3 -c "print(int($(now) - $began < $LIMIT))")" = 1 ]; do
    reply=$(docker exec mr-a1 /slates volume placed "$VOLUME" --snapshot "$SNAPSHOT" --mirror --json 2>"$DIR/placed.err")
    placed=$(echo "$reply" | python3 -c 'import sys,json; r=json.load(sys.stdin); print(r["placed"], r["mirror_age_ns"])' 2>/dev/null)
    case "$placed" in
      True*) result="placed/$(echo "$placed" | python3 -c 'import sys; a=sys.stdin.read().split()[1]; print("-" if a=="None" else round(int(a)/1e6))')ms"; break ;;
    esac
    python3 -c 'import time;time.sleep(0.2)'
  done
  echo "round $round: $(python3 -c "print(round($(now) - $began, 2))")s/$result"
  [ "$result" = unplaced ] && echo "  last reply: $reply $(head -c 300 "$DIR/placed.err")
  stat: $(docker exec mr-a1 /slates volume stat "$VOLUME" --json 2>/dev/null | python3 -c 'import sys,json; print(json.load(sys.stdin).get("placed"))' 2>/dev/null)"
done
uptime
