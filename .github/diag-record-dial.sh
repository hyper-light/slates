#!/usr/bin/env bash
# Diagnostics only, never on main: the record dial's pinning test
# (a_peer_started_a_probe_timeout_after_its_first_dial_is_linked_within_formations_budget) on a Linux runner. CI's Test
# step stops at the first failing test binary, and main's crates/bridge-fuse/tests/owner_turn.rs:295 fails before
# slates-server's fleet binary runs. Here the test runs alone RUNS times, each its own process, with the Test step's
# environment, then the fleet binary whole (--no-fail-fast). A failed run's panic carries every node's record links,
# record dials, links waiting on discovery, demultiplexer counters and refusals.
set +e
label=${1:-record-dial}
runs=${RUNS:-20}
test=a_peer_started_a_probe_timeout_after_its_first_dial_is_linked_within_formations_budget
R=../results.txt
export SLATES_FLEET_TRACE=1 SLATES_TEST_HEDGE_STARVATION_SECS=20 SLATES_TEST_RESTART_SLOWDOWN=10 \
  SLATES_TEST_FLEET_DEADLINE_SECS=120
{
  echo "record dial pinning test ($label) on $(uname -m), $(nproc) CPUs; kernel $(uname -r)"
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

passed=0
failed=0
for i in $(seq 1 "$runs"); do
  started=$SECONDS
  timeout -k 30 10m cargo test -p slates-server --test fleet -- --exact "$test" --nocapture > "../run-$i.log" 2>&1
  code=$?
  if grep -a -q 'test result: ok. 1 passed' "../run-$i.log"; then
    passed=$((passed + 1))
    echo "run $i: pass in $((SECONDS - started)) s" | tee -a "$R"
  else
    failed=$((failed + 1))
    echo "run $i: FAIL (exit $code) in $((SECONDS - started)) s (run-$i.log)" | tee -a "$R"
    grep -a -A2 'panicked at' "../run-$i.log" | head -6 | cut -c1-30000 | sed 's/^/   /' | tee -a "$R"
  fi
done
echo "== pinning test: $passed passed, $failed failed of $runs" | tee -a "$R"

# The fleet binary whole, as CI's Test step runs it.
started=$SECONDS
timeout -k 60 60m cargo test -p slates-server --test fleet --no-fail-fast > ../fleet.log 2>&1
code=$?
echo "== fleet binary: exit $code in $((SECONDS - started)) s (fleet.log)" | tee -a "$R"
grep -a -E ' \.\.\. FAILED$|^test result:' ../fleet.log | sed 's/^/   /' | tee -a "$R"
grep -a -E 'panicked at ' ../fleet.log | sed -E 's/.*panicked at /   panicked at /' | sort | uniq -c | head -20 \
  | tee -a "$R"
exit 0
