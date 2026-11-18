#!/usr/bin/env bash
# Diagnostics only, never on main: three-node formation (start_mesh's settle) looped on macOS, at PR #11's head with the
# record dial diagnostics, so a formation that stalls fails with every node's record dials, links waiting on discovery,
# demultiplexer counters and refusals. The count is derived: 1 failing macOS fleet suite in 32, each suite starting 80
# formations, puts the rate near 1 in 2,560 formations; 7,680 formations without a failure (the rule of three, 3/p)
# bound it below that at 95%. Each process runs one suite's 80 formations; this shard runs PROCESSES of them.
set +e
label=${1:-formation}
shard=${SHARD:-1}
per_process=${FORMATIONS_PER_PROCESS:-80}
processes=${PROCESSES:-32}
budget_secs=${BUDGET_SECS:-19800}
R=../results.txt
export SLATES_FLEET_TRACE=1 SLATES_TEST_HEDGE_STARVATION_SECS=20 SLATES_TEST_RESTART_SLOWDOWN=10 \
  SLATES_TEST_FLEET_DEADLINE_SECS=120
# A bound where GNU timeout is absent (macOS): perl's alarm ends the run.
bounded() {
  local secs=$1
  shift
  perl -e 'alarm shift; exec @ARGV or die "exec: $!"' "$secs" "$@"
}
{
  echo "formation loop ($label), shard $shard, on $(uname -sm), $(getconf _NPROCESSORS_ONLN) CPUs; $(uname -r)"
  echo "commit $(git rev-parse HEAD), parent $(git rev-parse HEAD~1)"
  echo "plan: $processes processes x $per_process formations, within $budget_secs s"
} > "$R"

began=$SECONDS
cargo test -p slates-server --test fleet --no-run > ../build.log 2>&1
code=$?
echo "== build: exit $code in $((SECONDS - began)) s (build.log)" | tee -a "$R"
if [ "$code" -ne 0 ]; then
  grep -a -E '^error' -A12 ../build.log | head -60 | tee -a "$R"
  exit 0
fi

formed=0
failed=0
began=$SECONDS
for p in $(seq 1 "$processes"); do
  left=$((budget_secs - (SECONDS - began)))
  if [ "$left" -le 0 ]; then
    echo "budget spent before process $p" | tee -a "$R"
    break
  fi
  log="../proc-$p.log"
  started=$SECONDS
  SLATES_DIAG_FORMATIONS=$per_process bounded "$left" cargo test -p slates-server --test fleet -- --ignored \
    --exact diag_formation_loop --nocapture > "$log" 2>&1
  code=$?
  n=$(grep -a -c '^diag: formation [0-9]* formed' "$log")
  formed=$((formed + n))
  grep -a -E '^diag: formation [0-9]+ formed in [0-9]+ ms' "$log" | awk '{print $(NF-1)}' >> ../formed-ms.txt
  if grep -a -q 'test result: ok. 1 passed' "$log"; then
    echo "process $p: $n formed in $((SECONDS - started)) s" | tee -a "$R"
  else
    failed=$((failed + 1))
    echo "process $p: FAILED after $n formed (exit $code) in $((SECONDS - started)) s ($log)" | tee -a "$R"
    grep -a -A2 'panicked at' "$log" | head -6 | cut -c1-30000 | sed 's/^/   /' | tee -a "$R"
  fi
done
echo "== formations: $formed formed, $failed failed, in $((SECONDS - began)) s" | tee -a "$R"
if [ -s ../formed-ms.txt ]; then
  sort -n ../formed-ms.txt | awk '{v[NR]=$1} END {
    printf "== formation ms: n %d, p50 %d, p90 %d, p99 %d, max %d\n", NR, v[int(NR*0.5)>0?int(NR*0.5):1],
      v[int(NR*0.9)>0?int(NR*0.9):1], v[int(NR*0.99)>0?int(NR*0.99):1], v[NR] }' | tee -a "$R"
fi
exit 0
