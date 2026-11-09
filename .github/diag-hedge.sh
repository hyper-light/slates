#!/bin/bash
# The reworked hedge test: 10 runs at nice 19 beside two spinners a CPU, then its fault once, unloaded. Diagnostics.
set +e
R=../results.txt
cpus=$(nproc)
spinners=$((cpus * 2))
t=a_slow_first_round_candidate_is_hedged_after_the_measured_p95
echo "runner $(uname -m), $cpus CPUs, $spinners spinners, 10 runs, tests at nice 19, commit $(git rev-parse --short HEAD)" > $R
pids=()
for i in $(seq $spinners); do (while :; do :; done) & pids+=($!); done
pass=0; fail=0
for run in $(seq 10); do
  out=$(nice -n 19 cargo test --locked -q -p slates-server --test fleet -- --exact $t --nocapture 2>&1); code=$?
  if [ $code -eq 0 ]; then pass=$((pass+1)); else fail=$((fail+1)); fi
  echo "$t run $run exit $code load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  echo "$out" | grep -E -A2 '^hedge:|panicked' | cut -c1-1500 | tee -a $R
done
echo "== $t passed $pass failed $fail" | tee -a $R
kill "${pids[@]}" 2>/dev/null
f=crates/server/src/fleet.rs
o='fn hedge_delay_ns(latency: &LatencyWindow) -> u64 {'
if [ "$(grep -cF "$o" $f)" = 1 ]; then
  awk -v o="$o" '{print} $0==o {print "  if latency.len() < usize::MAX {"; print "    return 300_000_000_000;"; print "  }"}' $f > $f.new && mv $f.new $f
  out=$(cargo test --locked -q -p slates-server --test fleet -- --exact $t --nocapture 2>&1); code=$?
  echo "== hedge mutant exit $code" | tee -a $R
  echo "$out" | grep -E -A2 'panicked|^hedge:' | cut -c1-600 | head -12 | tee -a $R
else
  echo "== hedge mutant NOT APPLIED" | tee -a $R
fi
