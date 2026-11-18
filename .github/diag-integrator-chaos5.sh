#!/bin/bash
# DIAG (not for main): the live_chaos flows that failed in diag-integrator-chaos4-1 (run 44320680683, main 38de3d2a9) on
# either arch, twice each, capped at 900 s, then the nemesis at the three seeds capped at 1500 s; every run's whole log
# uploaded (file names without colons), a capped run's last 60 lines printed, the judge's whole report printed.
set +e
R=target/chaos-results.txt
mkdir -p target
ulimit -n 65536 2>/dev/null || ulimit -n "$(ulimit -Hn)"
echo "runner $(uname -m), $(nproc) CPUs, commit $(git rev-parse --short HEAD), nofile $(ulimit -n)" > $R
failed=0
run_one() {
  local name=$1 cap=$2 tag=${3//::/__}; shift 3
  local log=target/chaos-$tag.log began=$(date +%s)
  env SLATES_TEST_CLI=1 "$@" timeout $cap cargo test --locked --release -q -p slates-cli --test live_chaos -- --exact $name --nocapture --test-threads=1 > $log 2>&1
  local code=$?
  local ok=no; grep -q "test result: ok. 1 passed" $log && ok=yes
  echo "$tag exit $code passed $ok took $(( $(date +%s) - began ))s load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  grep -aE "chaos .*seed [0-9]+ \(SLATES_CHAOS_SEED" $log | cut -c1-200 | tee -a $R
  grep -aE "panicked at" -A12 $log | cut -c1-700 | tee -a $R
  grep -aE "Continuity: (held|[0-9]+ violations)|: Continuity: durable before" $log | sort | uniq -c | cut -c1-300 | tee -a $R
  [ $code -eq 124 ] && { echo "CAPPED at ${cap}s; its last 60 lines:"; tail -60 $log | cut -c1-400; } | tee -a $R
  [ "$ok" = yes ] || failed=$((failed+1))
}
for name in a_long_live_log_is_taken_over_whole a_member_that_never_held_the_log_never_stands_in_for_its_holder \
  a_seeded_nemesis_of_single_faults_keeps_every_register_invariant an_owner_partitioned_mid_stream_is_fenced_and_loses_nothing \
  an_owner_that_hears_nothing_while_it_still_speaks_is_superseded_and_serves_nothing_stale; do
  for round in 1 2; do run_one "$name" 900 "$name-$round"; done
done
for seed in 7236833171758868842 17036665960917068132 6876545201569229162; do
  run_one a_seeded_nemesis_of_single_faults_keeps_every_register_invariant 1500 "nemesis-seed-$seed" SLATES_CHAOS_SEED=$seed
done
echo "== failed $failed" | tee -a $R
[ $failed -eq 0 ]
