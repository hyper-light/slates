#!/bin/bash
# A-232's red (diagnostics only, never on main): the a232_red probe, 10 runs, its process in a cgroup of cpu.weight 1
# beside eight spinners a CPU, each in a sibling cgroup of its own at the default weight (100), as systemd units are.
# Red-1 put two spinners a CPU in one weight-100 cgroup: the probe kept 1/101 of the machine, calls took ~225 ms and
# none stalled in 20 runs; the fixtures' starvation (A-228) came from many default-weight units. Red-2 starved the
# probe from its start, and its machine profile was refused before any daemon existed; the probe now starts unstarved
# and is moved once its daemon answers. Red-3 moved timeout's pid instead of the probe's, so nothing was starved. Each run prints every call that failed
# Stalled and whether the daemon's own watchdog withheld a beat across it.
set +e
R=../results.txt
cg=/sys/fs/cgroup
cpus=$(nproc)
spinners=$((cpus * 8))
bin=$(cargo test --locked -p slates-client --test a232_red --no-run --message-format=json 2>/dev/null \
  | jq -r 'select(.profile.test == true and .target.name == "a232_red") | .executable' | tail -1)
{
  echo "runner $(uname -m), $cpus CPUs, $spinners spinners each in its own cgroup at cpu.weight 100, probe at cpu.weight 1, 10 runs"
  echo "commit $(git rev-parse HEAD), base $(git rev-parse HEAD~1)"
  echo "kernel $(uname -r); cgroup v2 root controllers: $(cat $cg/cgroup.subtree_control)"
  echo "probe binary: $bin"
} > $R
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
echo "weights: starved $(cat $cg/a232-starved/cpu.weight); spinner cgroups $(ls -d $cg/a232-spin-* | wc -l), each $(cat $cg/a232-spin-1/cpu.weight), holding $(cat $cg/a232-spin-*/cgroup.procs | wc -l) spinners" | tee -a $R
red=0; green=0; broken=0
for run in $(seq 10); do
  used_before=$(awk '/usage_usec/ {print $2}' $cg/a232-starved/cpu.stat)
  # The probe starts unstarved (red-2 starved its machine profile into a refusal before any daemon existed) and is
  # moved, every thread of it, into the starved cgroup once it says its daemon answers.
  log=../probe-$run.log
  timeout 900 "$bin" --nocapture --test-threads=1 > $log 2>&1 &
  probe=$!
  # The pid the probe names itself: `$!` is timeout's, and moving it left the probe where it was (red-3, 0 ms in the
  # starved cgroup, 10 of 10 runs).
  for wait in $(seq 600); do grep -q A232-READY $log && break; kill -0 $probe 2>/dev/null || break; sleep 1; done
  test_pid=$(sed -n 's/^A232-READY pid \([0-9]*\).*/\1/p' $log | head -1)
  echo $test_pid | sudo tee $cg/a232-starved/cgroup.procs > /dev/null
  echo "probe $test_pid moved after ${wait} s: $(grep -h '' /proc/$test_pid/cgroup 2>/dev/null)" | tee -a $R
  # Back out once it has said what it saw, so its daemon's teardown is not starved too.
  for wait in $(seq 900); do grep -q A232-SUMMARY $log && break; kill -0 $probe 2>/dev/null || break; sleep 1; done
  echo $test_pid | sudo tee $cg/cgroup.procs > /dev/null 2>&1
  wait $probe
  code=$?
  out=$(cat $log)
  used_after=$(awk '/usage_usec/ {print $2}' $cg/a232-starved/cpu.stat)
  live=$(echo "$out" | grep -c "LIVE: no beat withheld")
  if [ "$live" -gt 0 ]; then red=$((red+1)); elif [ $code -eq 0 ]; then green=$((green+1)); else broken=$((broken+1)); fi
  echo "== run $run exit $code, stalled-live calls $live, probe CPU $(( (used_after - used_before) / 1000 )) ms, load $(cut -d' ' -f1 /proc/loadavg)" | tee -a $R
  echo "$out" | grep -E 'A232-|slates daemon: (shard|the watchdog)|panicked|test result' | cut -c1-400 | tee -a $R
done
echo "== a232_red: red (a live daemon's call failed Stalled) $red, green $green, other failure $broken, of 10" | tee -a $R
kill "${pids[@]}" 2>/dev/null
