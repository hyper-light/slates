#!/bin/bash
# DIAG (not for main): g240's and g250's owed runs -- the store-destroyed flow 24 times and the candidate-killed flow 12
# times, release, one at a time, every run's summary and counters kept.
set +e
R=target/takeover-results.txt
ulimit -n 65536 2>/dev/null || ulimit -n "$(ulimit -Hn)"
echo "runner $(uname -m), $(nproc) CPUs, commit $(git rev-parse --short HEAD), nofile $(ulimit -n)" > $R
failed=0
for pair in a_candidates_store_destroyed_mid_stream_is_rebuilt_and_serves_a_takeover:24 a_candidate_killed_mid_stream_catches_up_and_its_copy_serves_a_takeover:12; do
  t=${pair%%:*}; n=${pair##*:}; pass=0; fail=0
  for run in $(seq $n); do
    began=$(date +%s)
    out=$(SLATES_TEST_CLI=1 cargo test --locked --release -q -p slates-cli --test live_chaos -- --exact $t --nocapture --test-threads=1 2>&1); code=$?
    if [ $code -eq 0 ]; then pass=$((pass+1)); else fail=$((fail+1)); fi
    echo "$t run $run exit $code took $(( $(date +%s) - began ))s load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
    echo "$out" | grep -E 'panicked|Continuity: (held|[0-9]+ violations)|takeover_waits|recovered_past_tolerance|taken_over_past_tolerance|serves in' -A1 | cut -c1-600 | tee -a $R
    echo "$out" > target/takeover-$t-run-$run.log
  done
  echo "== $t passed $pass failed $fail" | tee -a $R
  failed=$((failed+fail))
done
[ $failed -eq 0 ]
