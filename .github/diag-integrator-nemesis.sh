#!/bin/bash
# DIAG (not for main): the seeded nemesis replayed at the three seeds that failed before A-338 (x86_64 7236833171758868842,
# arm64 17036665960917068132, and 6876545201569229162), each seed once on this arch, release; every run's log kept.
set +e
R=target/nemesis-results.txt
mkdir -p target
ulimit -n 65536 2>/dev/null || ulimit -n "$(ulimit -Hn)"
echo "runner $(uname -m), $(nproc) CPUs, commit $(git rev-parse --short HEAD), nofile $(ulimit -n)" > $R
t=a_seeded_nemesis_of_single_faults_keeps_every_register_invariant
failed=0
for seed in 7236833171758868842 17036665960917068132 6876545201569229162; do
  began=$(date +%s)
  log=target/nemesis-$seed.log
  SLATES_TEST_CLI=1 SLATES_CHAOS_SEED=$seed timeout 1800 cargo test --locked --release -q -p slates-cli --test live_chaos -- --exact $t --nocapture --test-threads=1 > $log 2>&1; code=$?
  ok=no; grep -q "test result: ok. 1 passed" $log && ok=yes
  echo "seed $seed exit $code passed $ok took $(( $(date +%s) - began ))s load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  grep -E "seed [0-9]+ \(SLATES_CHAOS_SEED|panicked|recovered at|Continuity: (held|[0-9]+ violations)" -A1 $log | cut -c1-700 | tee -a $R
  [ "$ok" = yes ] || failed=$((failed+1))
done
[ $failed -eq 0 ]
