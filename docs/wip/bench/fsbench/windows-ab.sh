#!/bin/bash
# Two builds of slates' WinFsp drive measured against each other (audit plan step 7): each build runs its own anchor,
# daemon, volume and drive at the same time, and one mount_bench process rounds over both drives, its first arm rotating
# each round, so a load that drifts lands on both alike. Every run makes both daemons afresh, and the build started
# first alternates between runs. crates/cli/examples/mount_bench.rs times every call, quiet and then loaded.
#
# Needs WinFsp installed and three release binaries. In Git Bash:
#
#   OLD_SLATES=old/slates.exe NEW_SLATES=new/slates.exe BENCH=new/mount_bench.exe \
#     OLD_NAME=dll NEW_NAME=receiver RUNS=5 SPINNERS_PER_CPU=2 bash docs/wip/bench/fsbench/windows-ab.sh
#
# Each run ends with the line `## run N of M done` (or `FAILED`), and AFTER_RUN, if set, is a command run then (CI publishes the
# output so far with it). FILES and ROUNDS shrink the workload for a smoke run; recorded numbers use the program's
# defaults. Each daemon's error output is printed after its run.
set -euo pipefail
: "${OLD_SLATES:?the older build's slates.exe}" "${NEW_SLATES:?the newer build's slates.exe}" "${BENCH:?mount_bench.exe}"
OLD_NAME=${OLD_NAME:-old}
NEW_NAME=${NEW_NAME:-new}
RUNS=${RUNS:-5}
SPINNERS_PER_CPU=${SPINNERS_PER_CPU:-2}
W=${RUNNER_TEMP:-${TEMP:-.}}/fsbench-ab-$$
mkdir -p "$W"

# The pid, instance, drive and binary of each side of the current run; both sides are torn down however a run ends.
OLD_ANCHOR= NEW_ANCHOR= OLD_DRIVE= NEW_DRIVE= OLD_INSTANCE= NEW_INSTANCE=

teardown() {
  set +e
  if [ -n "$OLD_DRIVE" ] && [ -e "$OLD_DRIVE/" ]; then "$OLD_SLATES" --instance "$OLD_INSTANCE" unmount "$OLD_DRIVE"; fi
  if [ -n "$NEW_DRIVE" ] && [ -e "$NEW_DRIVE/" ]; then "$NEW_SLATES" --instance "$NEW_INSTANCE" unmount "$NEW_DRIVE"; fi
  sleep 3 # DIAG: the worker prints its phases when it sees its stop
  if [ -n "$OLD_ANCHOR" ]; then kill "$OLD_ANCHOR" 2>/dev/null; wait "$OLD_ANCHOR" 2>/dev/null; fi
  if [ -n "$NEW_ANCHOR" ]; then kill "$NEW_ANCHOR" 2>/dev/null; wait "$NEW_ANCHOR" 2>/dev/null; fi
  OLD_ANCHOR= NEW_ANCHOR= OLD_DRIVE= NEW_DRIVE=
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
  for letter in S T U V W X Y Z; do
    if [ ! -e "$letter:/" ] && [ "$letter:" != "$OLD_DRIVE" ] && [ "$letter:" != "$NEW_DRIVE" ]; then
      echo "$letter:"
      return 0
    fi
  done
  echo "no free drive letter from S: to Z:" >&2
  return 1
}

# Starts one side's anchor and answers once its daemon does; the anchor's pid goes in the variable it names.
start_anchor() {
  local binary=$1 instance=$2 pid_variable=$3
  "$binary" --instance "$instance" anchor 2> "$W/$instance.err" &
  printf -v "$pid_variable" '%s' "$!"
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

# Makes one side's bounded volume and mounts it on the drive named. 256 MiB as windows.sh: a round holds about 6.5 MB.
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

start_old() {
  OLD_INSTANCE=fsb-$$-$1-old
  start_anchor "$OLD_SLATES" "$OLD_INSTANCE" OLD_ANCHOR
  OLD_DRIVE=$(free_drive)
  mount_volume "$OLD_SLATES" "$OLD_INSTANCE" "$OLD_DRIVE"
}

start_new() {
  NEW_INSTANCE=fsb-$$-$1-new
  start_anchor "$NEW_SLATES" "$NEW_INSTANCE" NEW_ANCHOR
  NEW_DRIVE=$(free_drive)
  mount_volume "$NEW_SLATES" "$NEW_INSTANCE" "$NEW_DRIVE"
}

for run in $(seq 1 "$RUNS"); do
  echo "## run $run of $RUNS, $(date -u +%FT%TZ)"
  if [ $((run % 2)) -eq 1 ]; then
    start_old "$run"
    start_new "$run"
    echo "# $OLD_NAME started first: $OLD_NAME=$OLD_DRIVE $NEW_NAME=$NEW_DRIVE"
  else
    start_new "$run"
    start_old "$run"
    echo "# $NEW_NAME started first: $OLD_NAME=$OLD_DRIVE $NEW_NAME=$NEW_DRIVE"
  fi
  # A run that fails is said to have failed and its logs printed, and the next run goes on: under load the anchor may
  # kill a daemon that missed its heartbeat, and a failed run is a result to record, not a reason to lose the rest.
  outcome=done
  "$BENCH" --arm "$OLD_NAME=$OLD_DRIVE/" --arm "$NEW_NAME=$NEW_DRIVE/" --spinners-per-cpu "$SPINNERS_PER_CPU" \
    ${FILES:+--files "$FILES"} ${ROUNDS:+--rounds "$ROUNDS"} ${PASS:+--pass "$PASS"} || outcome=FAILED
  # After the teardown, so a daemon's last words are in it.
  teardown
  for side in "$OLD_INSTANCE" "$NEW_INSTANCE"; do
    echo "# $side's error output, $(wc -l < "$W/$side.err") lines:"
    cat "$W/$side.err"
  done
  rm -f "$W/$OLD_INSTANCE.err" "$W/$NEW_INSTANCE.err"
  echo "## run $run of $RUNS $outcome, $(date -u +%FT%TZ)"
  if [ -n "${AFTER_RUN:-}" ]; then "$AFTER_RUN"; fi
done
