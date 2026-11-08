#!/bin/bash
# Builds of slates' WinFsp drive measured against each other (audit plan step 7): each build runs its own anchor, daemon,
# volume and drive at the same time, and one mount_bench process rounds over every drive, its first arm rotating each
# round, so a load that drifts lands on every arm alike. Every run makes every daemon afresh, and the build started first
# rotates between runs. crates/cli/examples/mount_bench.rs times every call, quiet and then loaded.
#
# Needs WinFsp installed, a release slates.exe for each arm and mount_bench.exe. In Git Bash:
#
#   ARMS="dll=dll/slates.exe receiver=new/slates.exe" BENCH=new/mount_bench.exe RUNS=5 SPINNERS_PER_CPU=2 \
#     bash docs/wip/bench/fsbench/windows-ab.sh
#
# ARMS is space-separated NAME=PATH pairs, two or more; a name is a word. Each run ends with the line `## run N of M done`
# (or `FAILED`), and AFTER_RUN, if set, is a command run then (CI publishes the output so far with it). FILES and ROUNDS
# shrink the workload for a smoke run; recorded numbers use the program's defaults. Each daemon's error output is printed
# after its run.
set -euo pipefail
: "${ARMS:?NAME=PATH pairs, one per build}" "${BENCH:?mount_bench.exe}"
RUNS=${RUNS:-5}
SPINNERS_PER_CPU=${SPINNERS_PER_CPU:-2}
W=${RUNNER_TEMP:-${TEMP:-.}}/fsbench-ab-$$
mkdir -p "$W"

NAMES=() BINARIES=()
for arm in $ARMS; do
  NAMES+=("${arm%%=*}")
  BINARIES+=("${arm#*=}")
done
if [ "${#NAMES[@]}" -lt 2 ]; then
  echo "ARMS names ${#NAMES[@]} build(s); an A/B needs two or more" >&2
  exit 1
fi

# Each arm's anchor pid, instance and drive in the current run; every arm is torn down however a run ends.
ANCHORS=() INSTANCES=() DRIVES=()

teardown() {
  set +e
  local index
  for index in "${!NAMES[@]}"; do
    local drive=${DRIVES[$index]:-} instance=${INSTANCES[$index]:-} anchor=${ANCHORS[$index]:-}
    if [ -n "$drive" ] && [ -e "$drive/" ]; then "${BINARIES[$index]}" --instance "$instance" unmount "$drive"; fi
    if [ -n "$anchor" ]; then kill "$anchor" 2>/dev/null; wait "$anchor" 2>/dev/null; fi
  done
  ANCHORS=() DRIVES=()
  set -e
}

cleanup() {
  teardown
  # A run that ended early has not printed its daemons' error output; a finished run removed each file after printing it.
  for left in "$W"/*.err; do
    if [ -e "$left" ]; then
      echo "# $left, left by a run that did not finish:"
      cat "$left"
    fi
  done
  rm -rf "$W"
}
trap cleanup EXIT

free_drive() {
  local letter taken drive
  for letter in S T U V W X Y Z; do
    taken=
    for drive in ${DRIVES[@]+"${DRIVES[@]}"}; do
      if [ "$drive" = "$letter:" ]; then taken=1; fi
    done
    if [ ! -e "$letter:/" ] && [ -z "$taken" ]; then
      echo "$letter:"
      return 0
    fi
  done
  echo "no free drive letter from S: to Z:" >&2
  return 1
}

# Starts arm `index`'s anchor on `instance`, in this shell so the anchor is its child (teardown waits for it, and no
# command substitution holds its output open), and answers once its daemon does; the pid goes in ANCHORS.
start_anchor() {
  local index=$1 instance=$2 binary=${BINARIES[$1]}
  "$binary" --instance "$instance" anchor 2> "$W/$instance.err" &
  ANCHORS[$index]=$!
  for _ in $(seq 1 600); do
    if "$binary" --instance "$instance" volume list > /dev/null 2>&1; then
      "$binary" --instance "$instance" bootstrap root > /dev/null 2>&1 || true
      return 0
    fi
    sleep 0.1
  done
  echo "$instance: the daemon never answered" >&2
  cat "$W/$instance.err" >&2
  return 1
}

# Makes one arm's bounded volume and mounts it on the drive named. 256 MiB as windows.sh: a round holds about 6.5 MB.
mount_volume() {
  local binary=$1 instance=$2 drive=$3 volume
  volume=$("$binary" --instance "$instance" volume create fsb --bounded 256MiB | tr -d '\r' | awk '$1 == "id:" { print $2 }')
  [ -n "$volume" ] || { echo "$instance: volume create printed no id" >&2; return 1; }
  "$binary" --instance "$instance" mount "$volume" "$drive" > /dev/null
  for _ in $(seq 1 600); do
    if [ -d "$drive/" ]; then return 0; fi
    sleep 0.1
  done
  echo "$instance: $drive never appeared" >&2
  return 1
}

# Starts arm `index` for run `run`: its anchor, daemon, volume and drive.
start_arm() {
  local index=$1 run=$2 instance drive
  instance=fsb-$$-$run-${NAMES[$index]}
  INSTANCES[$index]=$instance
  start_anchor "$index" "$instance"
  drive=$(free_drive)
  DRIVES[$index]=$drive
  mount_volume "${BINARIES[$index]}" "$instance" "$drive"
}

for run in $(seq 1 "$RUNS"); do
  echo "## run $run of $RUNS, $(date -u +%FT%TZ)"
  count=${#NAMES[@]}
  first=$(( (run - 1) % count ))
  for offset in $(seq 0 $((count - 1))); do
    start_arm $(( (first + offset) % count )) "$run"
  done
  arms=() shown=
  # The list the bench takes each round's arms from starts with this run's first arm, so every arm is listed first as
  # often as it starts first. With the list fixed, the arm listed first ran 3.15% under its run's mean and the last 3.90%
  # over in arm64's quiet pass, between copies of one binary (diag-winfsp-overlap-48, BENCHMARKS).
  for offset in "${!NAMES[@]}"; do
    index=$(( (first + offset) % count ))
    arms+=(--arm "${NAMES[$index]}=${DRIVES[$index]}/")
    shown="$shown ${NAMES[$index]}=${DRIVES[$index]}"
  done
  echo "# ${NAMES[$first]} started first:$shown"
  # A run that fails is said to have failed and its logs printed, and the next run goes on: under load the anchor may
  # kill a daemon that missed its heartbeat, and a failed run is a result to record, not a reason to lose the rest.
  outcome=done
  "$BENCH" "${arms[@]}" --spinners-per-cpu "$SPINNERS_PER_CPU" \
    ${FILES:+--files "$FILES"} ${ROUNDS:+--rounds "$ROUNDS"} || outcome=FAILED
  # After the teardown, so a daemon's last words are in it.
  teardown
  for instance in ${INSTANCES[@]+"${INSTANCES[@]}"}; do
    echo "# $instance's error output, $(wc -l < "$W/$instance.err") lines:"
    cat "$W/$instance.err"
    rm -f "$W/$instance.err"
  done
  echo "## run $run of $RUNS $outcome, $(date -u +%FT%TZ)"
  if [ -n "${AFTER_RUN:-}" ]; then "$AFTER_RUN"; fi
done
