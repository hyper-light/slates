#!/bin/bash
# DIAG (not for main): the failure rate of the anchor's serve-then-kill test, each run of the whole binary bounded at
# 300 s, every run's log kept, a failing run's panic printed.
set +e
R=target/anchor-results.txt
mkdir -p target
echo "runner $(uname -m), $(nproc) CPUs, commit $(git rev-parse --short HEAD)" > $R
run_set() {
  local label=$1 count=$2 pass=0 fail=0
  for i in $(seq 1 "$count"); do
    local log=target/anchor-$label-$i.log began=$(date +%s)
    timeout 300 cargo test --locked -q -p slates-anchor --test anchor > "$log" 2>&1
    local code=$?
    if [ $code -eq 0 ]; then pass=$((pass+1)); else fail=$((fail+1)); fi
    echo "$label run $i exit $code took $(( $(date +%s) - began ))s load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
    grep -aE "^test .* FAILED|panicked at" -A4 "$log" | cut -c1-400 | tee -a $R
  done
  echo "== $label: $pass passed, $fail failed of $count" | tee -a $R
}
run_set plain 30
spinners=()
for c in $(seq 1 "$(nproc)"); do (while :; do :; done) & spinners+=($!); done
echo "spinners: ${spinners[*]}" | tee -a $R
run_set spinners 20
kill "${spinners[@]}" 2>/dev/null
wait 2>/dev/null
echo "spinners left: $(for p in "${spinners[@]}"; do kill -0 "$p" 2>/dev/null && echo "$p"; done | wc -l)" | tee -a $R
grep "^== " $R
