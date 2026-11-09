#!/bin/bash
# The green-share takeover test, 10 runs at nice 19 beside two spinners a CPU, its takeover diagnostics kept. Diagnostics.
set +e
R=../results.txt
cpus=$(nproc)
spinners=$((cpus * 2))
t=a_share_of_a_green_made_after_its_last_version_survives_a_takeover
echo "runner $(uname -m), $cpus CPUs, $spinners spinners, 10 runs, tests at nice 19, commit $(git rev-parse --short HEAD)" > $R
pids=()
for i in $(seq $spinners); do (while :; do :; done) & pids+=($!); done
pass=0; fail=0
for run in $(seq 10); do
  began=$(date +%s)
  out=$(nice -n 19 cargo test --locked -q -p slates-server --test fleet -- --exact $t --nocapture 2>&1); code=$?
  if [ $code -eq 0 ]; then pass=$((pass+1)); else fail=$((fail+1)); fi
  echo "$t run $run exit $code took $(( $(date +%s) - began ))s load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  echo "$out" | grep -E '^survivor |panicked|TakeoverIncomplete|NotRecovered' -A1 | cut -c1-2500 | tee -a $R
done
echo "== $t passed $pass failed $fail" | tee -a $R
kill "${pids[@]}" 2>/dev/null
