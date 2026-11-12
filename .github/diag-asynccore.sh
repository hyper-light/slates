#!/bin/bash
# The client's async_core suite, 10 runs at nice 19 beside two spinners a CPU: which wall wait fails. Diagnostics.
set +e
R=../results.txt
cpus=$(nproc)
spinners=$((cpus * 2))
echo "runner $(uname -m), $cpus CPUs, $spinners spinners, 10 runs, tests at nice 19, commit $(git rev-parse --short HEAD)" > $R
pids=()
for i in $(seq $spinners); do (while :; do :; done) & pids+=($!); done
pass=0; fail=0
for run in $(seq 10); do
  out=$(nice -n 19 cargo test --locked -q -p slates-client --test async_core -- --nocapture 2>&1); code=$?
  if [ $code -eq 0 ]; then pass=$((pass+1)); else fail=$((fail+1)); fi
  echo "async_core run $run exit $code load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  echo "$out" | grep -E 'panicked|--- FAILED|test result' -A2 | cut -c1-600 | tee -a $R
done
echo "== async_core passed $pass failed $fail" | tee -a $R
kill "${pids[@]}" 2>/dev/null
