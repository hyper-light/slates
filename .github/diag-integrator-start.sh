#!/bin/bash
# DIAG (not for main): g254's owed run (A-311, A-312): a_start_whose_shards_sum_past_a_window_comes_up, 5 runs at nice 19
# beside two spinners a CPU; every anchor kill line kept.
set +e
R=target/start-results.txt
cpus=$( (nproc 2>/dev/null) || sysctl -n hw.ncpu)
spinners=$((cpus * 2))
t=a_start_whose_shards_sum_past_a_window_comes_up
load() { (cut -d' ' -f1 /proc/loadavg 2>/dev/null) || sysctl -n vm.loadavg; }
echo "runner $(uname -sm), $cpus CPUs, $spinners spinners, 5 runs, tests at nice 19, commit $(git rev-parse --short HEAD)" > $R
pids=()
for i in $(seq $spinners); do (while :; do :; done) & pids+=($!); done
pass=0; fail=0
for run in $(seq 5); do
  began=$(date +%s)
  out=$(SLATES_TEST_CLI=1 nice -n 19 cargo test --locked -q -p slates-cli --test cli -- --exact $t --nocapture 2>&1); code=$?
  if [ $code -eq 0 ]; then pass=$((pass+1)); else fail=$((fail+1)); fi
  echo "$t run $run exit $code took $(( $(date +%s) - began ))s load $(load)" | tee -a $R
  echo "$out" | grep -E 'skipping|a start of 1 shard|killed|kill|panicked|test result' -A1 | cut -c1-2000 | tee -a $R
  echo "$out" > target/start-run-$run.log
done
echo "== $t passed $pass failed $fail" | tee -a $R
kill "${pids[@]}" 2>/dev/null
[ $fail -eq 0 ]
