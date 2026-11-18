#!/bin/bash
# DIAG (not for main): g245's two fleet tests (a dead lent session), 10 runs each at normal priority beside one spinner a CPU.
set +e
R=target/lent-results.txt
cpus=$(nproc)
spinners=$cpus
echo "runner $(uname -m), $cpus CPUs, $spinners spinners, 10 runs each, tests at nice 0, commit $(git rev-parse --short HEAD)" > $R
pids=()
for i in $(seq $spinners); do (while :; do :; done) & pids+=($!); done
failed=0
for t in a_read_through_a_dead_session_keeps_its_place_behind_a_burst a_forward_lane_leaves_a_session_that_hears_nothing_for_a_new_one; do
  pass=0; fail=0
  for run in $(seq 10); do
    began=$(date +%s)
    out=$(SLATES_FLEET_TRACE=1 SLATES_TEST_FLEET_DEADLINE_SECS=120 cargo test --locked -q -p slates-server --test fleet -- --exact $t --nocapture 2>&1); code=$?
    if [ $code -eq 0 ]; then pass=$((pass+1)); else fail=$((fail+1)); fi
    echo "$t run $run exit $code took $(( $(date +%s) - began ))s load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
    echo "$out" | grep -E 'panicked' -A2 | cut -c1-2000 | tee -a $R
    echo "$out" > target/lent-$t-run-$run.log
  done
  echo "== $t passed $pass failed $fail" | tee -a $R
  failed=$((failed+fail))
done
kill "${pids[@]}" 2>/dev/null
[ $failed -eq 0 ]
