#!/bin/bash
# A-232 step 5 (diagnostics only, never on main): the rule and its bound live on Linux CI.
# 1. Stuck, supervised: liveness_tests' wedged case under the anchor that ships, 10 runs, the runner unloaded.
# 2. Stuck, unsupervised: lone_daemon's wedged case, 10 runs, the runner unloaded.
# 3. Starved, supervised: liveness_tests' starved case, 10 runs; once it names its anchor and daemon, both are moved
#    into a cgroup of cpu.weight 1 beside eight spinners a CPU, each in a sibling cgroup of its own at the default
#    weight (100), as systemd units are (A-232 step 1's red-4 shape), and moved back once it has said what it saw.
# Usage: diag-a232-step5.sh <label>   (MUTANT names a mutation run, recorded in the header)
set +e
label=${1:-green}
# A mutant's commit names the shapes it is a red-check of in this file; without it every shape runs.
shapes=$(cat .github/diag-a232-step5.shapes 2>/dev/null || echo "supervised unsupervised frozen starved")
runs() { case " $shapes " in *" $1 "*) return 0;; *) return 1;; esac; }
R=../results.txt
cg=/sys/fs/cgroup
cpus=$(nproc)
spinners=$((cpus * 8))
exe() { jq -r "select(.profile.test == true and .target.name == \"$1\") | .executable" | tail -1; }
cli=$(cargo test --locked -p slates-cli --bin slates --no-run --message-format=json 2>/dev/null | exe slates)
lone=$(cargo test --locked -p slates-client --test lone_daemon --no-run --message-format=json 2>/dev/null | exe lone_daemon)
{
  echo "A-232 step 5 ($label) on $(uname -m), $cpus CPUs; kernel $(uname -r)"
  echo "commit $(git rev-parse HEAD), parent $(git rev-parse HEAD~1)"
  echo "cli tests: $cli; lone_daemon: $lone"
  echo "shapes: $shapes"
} > $R
supervised=liveness_tests::an_anchored_daemons_wedged_shard_is_ended_within_the_liveness_bound_and_its_call_answered_after
starved=liveness_tests::an_anchored_daemon_starved_beside_spinners_fails_no_call_and_is_never_killed
unsupervised=a_lone_daemons_wedged_shard_ends_its_process_and_the_call_ends_daemon_gone
frozen=liveness_tests::an_anchored_daemon_stopped_whole_is_killed_by_its_process_judgment_and_its_call_answered_after
tally() { # name, exit codes...
  local name=$1; shift; local pass=0 fail=0
  for code in "$@"; do if [ "$code" -eq 0 ]; then pass=$((pass+1)); else fail=$((fail+1)); fi; done
  echo "== $name: passed $pass, failed $fail, of $#" | tee -a $R
}
if runs supervised; then
codes=()
for run in $(seq 10); do
  log=../supervised-$run.log
  timeout 300 "$cli" --exact $supervised --nocapture --test-threads=1 > $log 2>&1
  codes+=($?)
  echo "== supervised run $run exit ${codes[-1]}, load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  grep -hE 'A232-SUPERVISED|panicked|test result' $log | cut -c1-1500 | tee -a $R
done
tally "stuck, supervised" "${codes[@]}"
fi
if runs unsupervised; then
codes=()
for run in $(seq 10); do
  log=../unsupervised-$run.log
  timeout 300 "$lone" --exact $unsupervised --nocapture --test-threads=1 > $log 2>&1
  codes+=($?)
  echo "== unsupervised run $run exit ${codes[-1]}, load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  grep -hE 'A232-UNSUPERVISED|panicked|test result' $log | cut -c1-1500 | tee -a $R
done
tally "stuck, unsupervised" "${codes[@]}"
fi
if runs frozen; then
codes=()
for run in $(seq 10); do
  log=../frozen-$run.log
  timeout 300 "$cli" --exact $frozen --nocapture --test-threads=1 > $log 2>&1
  codes+=($?)
  echo "== frozen run $run exit ${codes[-1]}, load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  grep -hE 'A232-FROZEN|panicked|test result' $log | cut -c1-700 | tee -a $R
done
tally "frozen, supervised" "${codes[@]}"
fi
runs starved || exit 0
grep -qw cpu $cg/cgroup.subtree_control || echo +cpu | sudo tee $cg/cgroup.subtree_control > /dev/null
sudo mkdir -p $cg/a232-starved
echo 1 | sudo tee $cg/a232-starved/cpu.weight > /dev/null
pids=()
for i in $(seq $spinners); do
  sudo mkdir -p $cg/a232-spin-$i
  echo 100 | sudo tee $cg/a232-spin-$i/cpu.weight > /dev/null
  (while :; do :; done) &
  pids+=($!)
  echo $! | sudo tee $cg/a232-spin-$i/cgroup.procs > /dev/null
done
echo "weights: starved $(cat $cg/a232-starved/cpu.weight); $(ls -d $cg/a232-spin-* | wc -l) spinner cgroups at $(cat $cg/a232-spin-1/cpu.weight) holding $(cat $cg/a232-spin-*/cgroup.procs | wc -l) spinners" | tee -a $R
codes=()
for run in $(seq 10); do
  used_before=$(awk '/usage_usec/ {print $2}' $cg/a232-starved/cpu.stat)
  log=../starved-$run.log
  SLATES_A232_STARVE=1 SLATES_A232_STARVED_CGROUP=/a232-starved timeout 900 "$cli" --exact $starved --nocapture \
    --test-threads=1 > $log 2>&1 &
  probe=$!
  for wait in $(seq 600); do grep -q 'A232-STARVE pids' $log && break; kill -0 $probe 2>/dev/null || break; sleep 1; done
  moved=$(sed -n 's/.*A232-STARVE pids \([0-9]*\) \([0-9]*\).*/\1 \2/p' $log | head -1)
  for pid in $moved; do echo $pid | sudo tee $cg/a232-starved/cgroup.procs > /dev/null; done
  echo "moved $moved after ${wait} s: $(for pid in $moved; do grep -h '' /proc/$pid/cgroup 2>/dev/null; done | tr '\n' ' ')" | tee -a $R
  for wait in $(seq 900); do grep -q A232-STARVED-LINES $log && break; kill -0 $probe 2>/dev/null || break; sleep 1; done
  for pid in $moved; do echo $pid | sudo tee $cg/cgroup.procs > /dev/null 2>&1; done
  wait $probe
  codes+=($?)
  used_after=$(awk '/usage_usec/ {print $2}' $cg/a232-starved/cpu.stat)
  echo "== starved run $run exit ${codes[-1]}, starved CPU $(( (used_after - used_before) / 1000 )) ms, load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  grep -hE 'A232-(SUMMARY|FAILED|STARVED-LINES)|slates daemon: shard [0-9]+ is|heartbeat lapsed|panicked|test result' $log | cut -c1-600 | tee -a $R
done
tally "starved, supervised" "${codes[@]}"
kill "${pids[@]}" 2>/dev/null
