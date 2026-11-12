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
    # diag-50: whether a daemon that printed nothing at its end crashed or was killed here.
    if [ -n "$drive" ] && [ -e "$drive/" ]; then
      local said status
      said=$("${BINARIES[$index]}" --instance "$instance" unmount "$drive" 2>&1)
      status=$?
      echo "# teardown: unmount $drive of $instance answered exit $status: $(echo "$said" | tr -d '\r' | tr '\n' ' ' | cut -c1-300)"
    else
      echo "# teardown: $instance's drive '$drive' was gone before its unmount"
    fi
    if [ -n "$anchor" ]; then
      echo "# teardown: $instance's processes before its anchor is ended:"
      powershell -NoProfile -Command "Get-CimInstance Win32_Process -Filter \"Name='slates.exe'\" | Where-Object { \$_.CommandLine -like '*$instance*' } | ForEach-Object { '  pid {0}, parent {1}: {2}' -f \$_.ProcessId, \$_.ParentProcessId, \$_.CommandLine }" 2>&1 | tr -d '\r'
      kill "$anchor" 2>/dev/null; wait "$anchor" 2>/dev/null
      echo "# teardown: $instance's anchor ended (wait answered $?)"
    fi
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

# The stall detector (diag-winfsp-overlap-48's windows-latest lane went 1 h 40 m without output and was found only when
# its job ran out). Every step of a run says what it is doing (`beat`), and the bench prints a line as each round starts;
# a run that shows neither for STALL_AFTER_S is stalled: the watcher prints what the run was doing, each daemon's
# status, every slates and bench thread's state, and dumps those processes, then ends the run, and the lane fails.
# Derived: the slowest normal run recorded in these lanes, 19 min 19 s for four arms on windows-11-arm
# (diag-winfsp-overlap-44, run 42404256322, run 4 of 5): no step and no round of a run that finished ever went quiet
# that long, and a stall then costs at most that, not the job's 355 minutes.
# STALL_AFTER_S shortens it for a smoke run, as FILES and ROUNDS shrink the workload; recorded runs use this one.
STALL_AFTER_S=${STALL_AFTER_S:-$(( 19 * 60 + 19 ))}
# Shape: how often the watcher looks; a stall is reported within this of its bound.
STALL_POLL_S=5
# Shape: the longest the watcher waits for one capture (a status from a daemon that may be wedged, a dump).
STALL_CAPTURE_S=120
# Where the dumps go: the workspace, so CI can keep them as an artifact.
STALL_DUMPS=${GITHUB_WORKSPACE:-$PWD}/stall-dumps

beat() { echo "$(date -u +%FT%TZ) $*" > "$W/beat"; }

# The newest of the run's two signs of life, in seconds since the epoch.
last_sign() {
  local newest=0 file seen
  for file in "$W/beat" "$W/bench.log"; do
    if [ -e "$file" ]; then
      seen=$(date -r "$file" +%s)
      if [ "$seen" -gt "$newest" ]; then newest=$seen; fi
    fi
  done
  echo "$newest"
}

stall_report() {
  local quiet=$1
  echo "## STALL: no step and no round for ${quiet} s (bound ${STALL_AFTER_S} s)"
  echo "# the run's last step: $(cat "$W/beat" 2>/dev/null)"
  echo "# the bench's last round: $(grep '^progress:' "$W/bench.log" 2>/dev/null | tail -1)"
  local index binary instance anchor
  while read -r index binary instance anchor; do
    echo "# status of $instance (arm ${NAMES[$index]}):"
    /usr/bin/timeout "$STALL_CAPTURE_S" "$binary" --instance "$instance" status 2>&1 || echo "# (status of $instance did not answer within ${STALL_CAPTURE_S} s)"
  done < "$W/arms"
  mkdir -p "$STALL_DUMPS"
  /usr/bin/timeout "$STALL_CAPTURE_S" powershell -NoProfile -Command "
    \$dumps = '$(cygpath -w "$STALL_DUMPS")'
    Get-Process slates, mount_bench -ErrorAction SilentlyContinue | ForEach-Object {
      \$p = \$_
      '# threads of {0} {1}, started {2:o}, CPU {3}' -f \$p.ProcessName, \$p.Id, \$p.StartTime, \$p.TotalProcessorTime
      \$p.Threads | ForEach-Object {
        \$reason = if (\$_.ThreadState -eq 'Wait') { \$_.WaitReason } else { '' }
        '  thread {0}: {1} {2}, CPU {3}' -f \$_.Id, \$_.ThreadState, \$reason, \$_.TotalProcessorTime
      }
      \$file = Join-Path \$dumps ('{0}-{1}.dmp' -f \$p.ProcessName, \$p.Id)
      Start-Process rundll32.exe -ArgumentList ('C:\Windows\System32\comsvcs.dll, MiniDump {0} {1} full' -f \$p.Id, \$file) -Wait -NoNewWindow
      if (Test-Path \$file) { '  dumped {0}, {1} bytes' -f \$file, (Get-Item \$file).Length } else { '  no dump of {0}' -f \$p.Id }
    }
  " 2>&1 | tr -d '\r' || echo "# (the thread capture did not finish within ${STALL_CAPTURE_S} s)"
}

# Runs beside one run until it ends ("$W/run.done") or stalls; on a stall it reports, marks the run, and ends the bench
# and the anchors, so the run's own waits return.
watch_run() {
  local quiet
  while [ ! -e "$W/run.done" ]; do
    sleep "$STALL_POLL_S"
    quiet=$(( $(date +%s) - $(last_sign) ))
    if [ "$quiet" -ge "$STALL_AFTER_S" ]; then
      stall_report "$quiet" > "$W/stall.txt" 2>&1
      touch "$W/stalled"
      if [ -e "$W/bench.pid" ]; then kill "$(cat "$W/bench.pid")" 2>/dev/null; fi
      local index binary instance anchor
      while read -r index binary instance anchor; do kill "$anchor" 2>/dev/null; done < "$W/arms"
      return 0
    fi
  done
}

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
  echo "$index $binary $instance $!" >> "$W/arms"
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
  beat "run $run: starting $instance's anchor"
  start_anchor "$index" "$instance"
  drive=$(free_drive)
  DRIVES[$index]=$drive
  beat "run $run: making and mounting $instance's volume on $drive"
  mount_volume "${BINARIES[$index]}" "$instance" "$drive"
}

for run in $(seq 1 "$RUNS"); do
  echo "## run $run of $RUNS, $(date -u +%FT%TZ)"
  count=${#NAMES[@]}
  first=$(( (run - 1) % count ))
  rm -f "$W/arms" "$W/run.done" "$W/stalled" "$W/stall.txt" "$W/bench.log" "$W/bench.pid"
  beat "run $run: starting"
  watch_run &
  watcher=$!
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
  beat "run $run: the bench"
  "$BENCH" "${arms[@]}" --spinners-per-cpu "$SPINNERS_PER_CPU" \
    ${FILES:+--files "$FILES"} ${ROUNDS:+--rounds "$ROUNDS"} ${PASS:+--pass "$PASS"} 2> "$W/bench.log" &
  echo $! > "$W/bench.pid"
  wait "$(cat "$W/bench.pid")" || outcome=FAILED
  # The bench's own error output; its progress lines were for the stall detector.
  grep -v '^progress:' "$W/bench.log" >&2 || true
  # After the teardown, so a daemon's last words are in it.
  beat "run $run: teardown"
  teardown
  touch "$W/run.done"
  wait "$watcher" 2>/dev/null || true
  if [ -e "$W/stalled" ]; then
    cat "$W/stall.txt"
    outcome="FAILED (stalled)"
  fi
  for instance in ${INSTANCES[@]+"${INSTANCES[@]}"}; do
    echo "# $instance's error output, $(wc -l < "$W/$instance.err") lines:"
    cat "$W/$instance.err"
    rm -f "$W/$instance.err"
  done
  echo "## run $run of $RUNS $outcome, $(date -u +%FT%TZ)"
  if [ -n "${AFTER_RUN:-}" ]; then "$AFTER_RUN"; fi
  # A stall is the lane's failure, not a run to step past: it names itself above, and the dumps are in $STALL_DUMPS.
  if [ -e "$W/stalled" ]; then exit 1; fi
done
