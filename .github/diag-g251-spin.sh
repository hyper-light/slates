#!/bin/bash
# DIAG (not for main): the seeded nemesis at seed 17036665960917068132, three runs, release, with the watchdog's dump of
# every shard's pulse and the stuck shard's backtrace at a stuck verdict (g251, item 1).
set +e
R=target/spin-results.txt
mkdir -p target
ulimit -n 65536 2>/dev/null || ulimit -n "$(ulimit -Hn)"
echo "runner $(uname -m), $(nproc) CPUs, commit $(git rev-parse --short HEAD), nofile $(ulimit -n)" > $R
t=a_seeded_nemesis_of_single_faults_keeps_every_register_invariant
seed=17036665960917068132
for run in 1 2 3; do
  began=$(date +%s)
  log=target/spin-$run.log
  SLATES_TEST_CLI=1 SLATES_CHAOS_SEED=$seed timeout 1200 cargo test --locked --release -q -p slates-cli --test live_chaos -- --exact $t --nocapture --test-threads=1 > $log 2>&1; code=$?
  echo "run $run exit $code took $(( $(date +%s) - began ))s load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  grep -E "DIAG|spinning|wedged|panicked" $log | cut -c1-600 | head -60 | tee -a $R
done
true
