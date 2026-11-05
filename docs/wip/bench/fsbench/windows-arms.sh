#!/bin/bash
# Each build's WinFsp drive measured alone, in turn (the priority measurement of A-212; diagnostics). One daemon runs at a
# time, so no arm takes CPU from another: a cycle runs every arm once, and the order rotates each cycle. Every run makes its
# daemon afresh. mount_bench's spinners are a process of their own, whose rate alone and beside the arm it prints.
#
#   ARMS="dll=old/slates.exe normal=new/slates.exe raised=raised/slates.exe" BENCH=new/mount_bench.exe CYCLES=5 \
#     SPINNERS_PER_CPU=2 bash windows-arms.sh
set -uo pipefail
: "${ARMS:?NAME=slates.exe ...}" "${BENCH:?mount_bench.exe}"
CYCLES=${CYCLES:-5}
SPINNERS_PER_CPU=${SPINNERS_PER_CPU:-2}
W=${RUNNER_TEMP:-${TEMP:-.}}/fsbench-arms-$$
mkdir -p "$W"
read -r -a arms <<< "$ARMS"

run_arm() {
  local name=$1 binary=$2 cycle=$3 instance="fsb-$$-$3-$1" anchor drive volume outcome=done
  "$binary" --instance "$instance" anchor 2> "$W/$instance.err" &
  anchor=$!
  for _ in $(seq 1 600); do
    if "$binary" --instance "$instance" volume list > /dev/null 2>&1; then break; fi
    sleep 0.1
  done
  "$binary" --instance "$instance" bootstrap root > /dev/null 2>&1
  for letter in S T U V W X Y Z; do
    if [ ! -e "$letter:/" ]; then drive="$letter:"; break; fi
  done
  volume=$("$binary" --instance "$instance" volume create fsb --bounded 256MiB | tr -d '\r' | awk '$1 == "id:" { print $2 }')
  "$binary" --instance "$instance" mount "$volume" "$drive" > /dev/null
  for _ in $(seq 1 600); do
    if [ -d "$drive/" ]; then break; fi
    sleep 0.1
  done
  echo "## run $cycle of $CYCLES arm $name, $(date -u +%FT%TZ)"
  "$BENCH" --arm "$name=$drive/" --spinners-per-cpu "$SPINNERS_PER_CPU" \
    ${FILES:+--files "$FILES"} ${ROUNDS:+--rounds "$ROUNDS"} || outcome=FAILED
  if [ -e "$drive/" ]; then "$binary" --instance "$instance" unmount "$drive"; fi
  sleep 3
  kill "$anchor" 2>/dev/null
  wait "$anchor" 2>/dev/null
  echo "# $name's error output, heartbeat lines and lapses:"
  grep -E 'heartbeat|exited|reply spin window' "$W/$instance.err" | cut -c1-200
  rm -f "$W/$instance.err"
  echo "## run $cycle of $CYCLES arm $name $outcome, $(date -u +%FT%TZ)"
  if [ -n "${AFTER_RUN:-}" ]; then "$AFTER_RUN"; fi
}

count=${#arms[@]}
for cycle in $(seq 1 "$CYCLES"); do
  for offset in $(seq 0 $((count - 1))); do
    pair=${arms[$(((cycle - 1 + offset) % count))]}
    run_arm "${pair%%=*}" "${pair#*=}" "$cycle"
  done
done
rm -rf "$W"
