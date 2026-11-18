#!/bin/bash
# DIAG (not for main): A-292's two live FUSE fsync flows (g244), 10 runs each at nice 19 beside two spinners a CPU; every
# "a held fsync gave up" line kept.
set +e
R=target/fsync-results.txt
cpus=$(nproc)
spinners=$((cpus * 2))
echo "runner $(uname -m), $cpus CPUs, $spinners spinners, 10 runs each, tests at nice 19, commit $(git rev-parse --short HEAD), nofile $(ulimit -n)" > $R
pids=()
for i in $(seq $spinners); do (while :; do :; done) & pids+=($!); done
failed=0
for t in a_fuse_fsync_on_a_live_volume_answers_only_once_a_candidate_holds_it a_fuse_fsync_on_a_live_volume_waits_out_candidates_that_answer_slowly; do
  pass=0; fail=0
  for run in $(seq 10); do
    began=$(date +%s)
    out=$(SLATES_TEST_CLI=1 nice -n 19 cargo test --locked --release -q -p slates-cli --test live_fleet -- --exact $t --nocapture 2>&1); code=$?
    if [ $code -eq 0 ]; then pass=$((pass+1)); else fail=$((fail+1)); fi
    echo "$t run $run exit $code took $(( $(date +%s) - began ))s load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
    echo "$out" | grep -E 'skipping|a held fsync gave up|panicked' -A2 | cut -c1-2500 | tee -a $R
    echo "$out" > target/fsync-$t-run-$run.log
  done
  echo "== $t passed $pass failed $fail" | tee -a $R
  failed=$((failed+fail))
done
kill "${pids[@]}" 2>/dev/null
[ $failed -eq 0 ]
