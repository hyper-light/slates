#!/bin/bash
# DIAG (not for main): see the workflow's header. Each run bounded at 120 s; every log kept.
set +e
R=target/anchorrace-results.txt
mkdir -p target
T=a_daemon_killed_after_it_served_is_restarted_every_time
echo "runner $(uname -m), $(nproc) CPUs, commit $(git rev-parse --short HEAD)" > $R
run_set() {
  local label=$1 pass=0 fail=0
  cargo test --locked -q -p slates-anchor --test anchor --no-run > target/anchorrace-build-$label.log 2>&1 || { echo "$label: build failed" | tee -a $R; tail -30 target/anchorrace-build-$label.log | tee -a $R; return; }
  for i in 1 2 3 4 5; do
    local log=target/anchorrace-$label-$i.log began=$(date +%s)
    timeout 120 cargo test --locked -q -p slates-anchor --test anchor -- --exact $T > "$log" 2>&1
    local code=$?
    if [ $code -eq 0 ]; then pass=$((pass+1)); else fail=$((fail+1)); fi
    echo "$label run $i exit $code took $(( $(date +%s) - began ))s" | tee -a $R
    grep -aE "panicked at" -A3 "$log" | cut -c1-300 | tee -a $R
  done
  echo "== $label: $pass passed, $fail failed of 5" | tee -a $R
}
run_set control
F=crates/anchor/src/supervise.rs
n=$(grep -c 'let child = self.spawn_child(env)?;' $F)
echo "injection sites: $n" | tee -a $R
sed -i 's|    let child = self.spawn_child(env)?;|    let child = self.spawn_child(env)?;\n    std::thread::sleep(std::time::Duration::from_millis(200));|' $F
grep -n -A1 'let child = self.spawn_child(env)?;' $F | tee -a $R
run_set injected
grep "^== " $R
