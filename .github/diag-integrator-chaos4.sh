#!/bin/bash
# DIAG (not for main): live_chaos, every flow on its own with a cap, then the seeded nemesis replayed at three seeds;
# each run's log streamed to its own file, a capped run's last 60 lines printed (the supervisor's ask of 2026-11-18
# 14:01 UTC, after A-343, A-344 and A-346).
set +e
R=target/chaos-results.txt
mkdir -p target
ulimit -n 65536 2>/dev/null || ulimit -n "$(ulimit -Hn)"
echo "runner $(uname -m), $(nproc) CPUs, commit $(git rev-parse --short HEAD), nofile $(ulimit -n)" > $R
names=$(cargo test --locked --release -q -p slates-cli --test live_chaos -- --list 2>/dev/null | sed -n 's/: test$//p')
echo "flows: $(echo "$names" | wc -l)" | tee -a $R
failed=0
run_one() {
  local name=$1 cap=$2 tag=$3; shift 3
  local log=target/chaos-$tag.log began=$(date +%s)
  env SLATES_TEST_CLI=1 "$@" timeout $cap cargo test --locked --release -q -p slates-cli --test live_chaos -- --exact $name --nocapture --test-threads=1 > $log 2>&1
  local code=$?
  local ok=no; grep -q "test result: ok. 1 passed" $log && ok=yes
  echo "$tag exit $code passed $ok took $(( $(date +%s) - began ))s load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  grep -aE "panicked at" -A2 $log | cut -c1-600 | tee -a $R
  [ $code -eq 124 ] && { echo "CAPPED at ${cap}s; its last 60 lines:"; tail -60 $log | cut -c1-400; } | tee -a $R
  [ "$ok" = yes ] || failed=$((failed+1))
}
for name in $names; do run_one "$name" 900 "$name"; done
for seed in 7236833171758868842 17036665960917068132 6876545201569229162; do
  run_one a_seeded_nemesis_of_single_faults_keeps_every_register_invariant 1500 "nemesis-seed-$seed" SLATES_CHAOS_SEED=$seed
done
echo "== failed $failed" | tee -a $R
[ $failed -eq 0 ]
