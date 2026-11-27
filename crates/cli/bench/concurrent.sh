# Concurrent clients for the A-699 harnesses (`git_scale.sh`, `hg_scale.sh`, `svn_scale.sh`; main 01a20182's ruling of
# 2026-11-27 15:02, A-699's owed "concurrent clients"): N stock clients at once against one server, each client's result
# checked as the harness checks its single client's, the wall time for all N, the per-client median and most, and the
# server's peak resident memory recorded. Sourced by a harness after it defines `bounded`, `RESULTS` and `WORK`.
#
# N is derived from the machine: one client is run first, sampled (the probe, untimed in the table), and N is the fewer
# of this machine's cores and the clients its available memory holds at the server's measured growth for that one
# client, at least 2; CONCURRENCY in the environment overrides it. A machine whose memory holds fewer than 2 such
# clients skips the step, saying so.
#
# A concurrent step's row is "SERVER concurrent-OP RUN WALL STATUS", a single-client row's shape, so the harness's table
# and its failure rules read it as one: STATUS is ok only when every client ended ok. Its figures are a line of their
# own, "SERVER concurrent-OP-figures RUN clients=N p50=P max=M peak=MiB", read by `concurrent_table`. Each client's
# check is a "SERVER check concurrent-OP client K ..." line, DIFFER or MISSED where it found the served data unlike
# what was expected, which fails the harness as a single client's check does.

# This machine's cores.
cores() { sysctl -n hw.ncpu 2> /dev/null || nproc; }

# The memory this machine has available now, in KiB: macOS's free, inactive and speculative pages; Linux's MemAvailable.
available_kib() {
  if [ -r /proc/meminfo ]; then
    awk '/^MemAvailable:/ { print $2 }' /proc/meminfo
  else
    vm_stat | awk -v page="$(sysctl -n hw.pagesize)" '
      /Pages free/ || /Pages inactive/ || /Pages speculative/ { gsub(/\./, "", $NF); pages += $NF }
      END { printf "%d\n", pages * page / 1024 }'
  fi
}

# The summed resident memory, in KiB, of the processes $@ and every process descended from them.
tree_rss_kib() {
  ps -A -o pid=,ppid=,rss= | awk -v roots="$*" '
    BEGIN { split(roots, r, " "); for (i in r) keep[r[i]] = 1 }
    { pid[NR] = $1; parent[NR] = $2; rss[NR] = $3 }
    END {
      grew = 1
      while (grew) { grew = 0; for (i = 1; i <= NR; i++) if (!(pid[i] in keep) && (parent[i] in keep)) { keep[pid[i]] = 1; grew = 1 } }
      for (i = 1; i <= NR; i++) if (pid[i] in keep) total += rss[i]
      printf "%d\n", total
    }'
}

# Samples the resident memory of the server processes $@ (with their descendants) every tenth of a second into
# WORK/peak.samples until `peak_stop`.
peak_start() {
  : > "$WORK/peak.samples"
  ( while true; do tree_rss_kib "$@" >> "$WORK/peak.samples"; sleep 0.1; done ) &
  PEAK_SAMPLER=$!
}

# Stops the sampler: the most sampled, in KiB.
peak_stop() {
  kill "$PEAK_SAMPLER" 2> /dev/null
  wait "$PEAK_SAMPLER" 2> /dev/null
  sort -n "$WORK/peak.samples" | tail -1
}

# The clients this machine runs at once for server step $1, given the server's resident growth for one client in KiB
# ($2): CONCURRENCY, else the fewer of the cores and the clients the available memory holds at that growth. Recorded.
concurrency() {
  local step=$1 growth=$2 avail n by_memory
  avail=$(available_kib)
  [ "$growth" -lt 1024 ] && growth=1024
  by_memory=$((avail / growth))
  n=$(cores)
  [ "$by_memory" -lt "$n" ] && n=$by_memory
  [ -n "${CONCURRENCY:-}" ] && n=$CONCURRENCY
  echo "concurrency of $step: $n, cores $(cores), available $((avail / 1024)) MiB, one client grew the server $((growth / 1024)) MiB" \
    | tee -a "$RESULTS" >&2
  echo "$n"
}

# Runs `concurrent-$2` for server $1's run $3 against the server processes in SERVER_PIDS: the client command is
# function $4, called as `$4 K` for client K (0 for the probe) in a shell of its own, given the variables CLIENT_VARS
# names (`declare -p`); its check runs after every client ended, in `$5 K` (a function writing the check line); `$6`,
# where given, prepares what N clients need before they start, untimed (`$6 N`, `$6 0` for the probe). C_DIR is the
# step's own directory. Records the row (header).
concurrent() {
  local server=$1 op=$2 run=$3 client=$4 checker=$5 prepare=${6:-} before peak growth n k start end
  C_DIR="$WORK/concurrent-$server-$op-$run"
  local dir=$C_DIR
  rm -rf "$dir" && mkdir -p "$dir"
  [ -n "$prepare" ] && "$prepare" 0
  # The probe: one client, the server sampled, its growth what N is derived from.
  before=$(tree_rss_kib $SERVER_PIDS)
  peak_start $SERVER_PIDS
  bounded bash -c "$(declare -f "$client"); $(declare -p C_DIR ${CLIENT_VARS:-} 2> /dev/null); $client 0" \
    > "$dir/probe.out" 2>&1 || true
  peak=$(peak_stop)
  growth=$((peak > before ? peak - before : 0))
  n=$(concurrency "$server concurrent-$op $run" "$growth")
  if [ "$n" -lt 2 ]; then
    echo "$server skipped concurrent-$op $run: the machine holds $n such clients" | tee -a "$RESULTS"
    return 0
  fi
  [ -n "$prepare" ] && "$prepare" "$n"
  before=$(tree_rss_kib $SERVER_PIDS)
  peak_start $SERVER_PIDS
  start=$(now)
  local clients=()
  for k in $(seq 1 "$n"); do
    (
      local_start=$(now)
      if bounded bash -c "$(declare -f "$client"); $(declare -p C_DIR ${CLIENT_VARS:-} 2> /dev/null); $client $k" \
        > "$dir/client-$k.out" 2> "$dir/client-$k.err"; then status=ok; else status="failed($?)"; fi
      echo "$(perl -e 'printf "%.3f", $ARGV[1] - $ARGV[0]' "$local_start" "$(now)") $status" > "$dir/client-$k.time"
    ) &
    clients+=($!)
  done
  # The clients alone: a bare `wait` waits for the sampler too, which runs until it is stopped.
  wait "${clients[@]}"
  end=$(now)
  peak=$(peak_stop)
  for k in $(seq 1 "$n"); do "$checker" "$k"; done
  sort -n "$dir"/client-*.time | awk -v n="$n" -v server="$server" -v op="$op" -v run="$run" \
    -v wall="$(perl -e 'printf "%.3f", $ARGV[1] - $ARGV[0]' "$start" "$end")" -v peak="$((peak / 1024))" '
      { time[NR] = $1; if ($2 != "ok") failed++ }
      END {
        p50 = time[int((NR + 1) / 2)]
        printf "%s concurrent-%s %s %s %s\n", server, op, run, wall, (failed ? "failed(" failed "-of-" n ")" : "ok")
        printf "%s concurrent-%s-figures %s clients=%d wall=%s p50=%s max=%s peak=%s\n", server, op, run, n, wall, p50,
          time[NR], peak
      }' | tee -a "$RESULTS"
  for k in $(seq 1 "$n"); do
    if ! grep -q ' ok$' "$dir/client-$k.time"; then tail -3 "$dir/client-$k.err" | sed "s/^/    client $k: /"; fi
  done
}

# The concurrent steps' figures, after the harness's table: each server's clients, wall time, median and most per
# client, and its peak resident memory, every run shown.
concurrent_table() {
  grep -qE '^[^ ]+ concurrent-[^ ]+-figures ' "$RESULTS" || return 0
  echo "== concurrent clients (each run: clients, wall s, per-client p50 and max s, server peak MiB)"
  grep -E '^[^ ]+ concurrent-[^ ]+-figures ' "$RESULTS" | awk '
    { step = $2; sub(/-figures$/, "", step); printf "%-22s %-12s run %s: %s %s %s %s %s\n", step, $1, $3, $4, $5, $6, $7, $8 }'
}
