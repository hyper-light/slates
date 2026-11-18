#!/usr/bin/env bash
# Diagnostics only, never on main: the record dial's pinning test
# (a_peer_started_a_probe_timeout_after_its_first_dial_is_linked_within_formations_budget) swept over how late the
# second and third nodes start (SLATES_DIAG_LATE_START_MS, a diag-only override of LATE_PEER_START_NS), RUNS runs a
# value, each its own process, with CI's Test-step environment. A failed run's panic carries every node's record links,
# record dials, links waiting on discovery, demultiplexer counters and refusals.
set +e
label=${1:-record-dial}
runs=${RUNS:-5}
late_starts=${LATE_STARTS_MS:-"667 2000 5000 10000 15000"}
test=a_peer_started_a_probe_timeout_after_its_first_dial_is_linked_within_formations_budget
R=../results.txt
export SLATES_FLEET_TRACE=1 SLATES_TEST_HEDGE_STARVATION_SECS=20 SLATES_TEST_RESTART_SLOWDOWN=10 \
  SLATES_TEST_FLEET_DEADLINE_SECS=120
# A bound on each run that works where GNU timeout is absent (macOS): perl's alarm ends the run.
bounded() {
  local secs=$1
  shift
  perl -e 'alarm shift; exec @ARGV or die "exec: $!"' "$secs" "$@"
}
{
  echo "record dial late-start sweep ($label) on $(uname -sm), $(getconf _NPROCESSORS_ONLN) CPUs; $(uname -r)"
  echo "commit $(git rev-parse HEAD), parent $(git rev-parse HEAD~1)"
} > "$R"

started=$SECONDS
cargo test -p slates-server --test fleet --no-run > ../build.log 2>&1
code=$?
echo "== build: exit $code in $((SECONDS - started)) s (build.log)" | tee -a "$R"
if [ "$code" -ne 0 ]; then
  grep -a -E '^error' -A12 ../build.log | head -60 | tee -a "$R"
  exit 0
fi

for late in $late_starts; do
  passed=0
  failed=0
  for i in $(seq 1 "$runs"); do
    log="../run-$late-$i.log"
    started=$SECONDS
    SLATES_DIAG_LATE_START_MS=$late bounded 600 cargo test -p slates-server --test fleet -- --exact "$test" \
      --nocapture > "$log" 2>&1
    code=$?
    if grep -a -q 'test result: ok. 1 passed' "$log"; then
      passed=$((passed + 1))
      echo "late $late ms run $i: pass in $((SECONDS - started)) s" | tee -a "$R"
    else
      failed=$((failed + 1))
      echo "late $late ms run $i: FAIL (exit $code) in $((SECONDS - started)) s ($log)" | tee -a "$R"
      grep -a -A2 'panicked at' "$log" | head -6 | cut -c1-30000 | sed 's/^/   /' | tee -a "$R"
    fi
  done
  echo "== late start $late ms: $passed passed, $failed failed of $runs" | tee -a "$R"
done
exit 0
