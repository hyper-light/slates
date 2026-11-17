#!/usr/bin/env bash
# Diagnostics only, never on main: the A-232 landing's Linux tests past main's known red. CI's Test step stops at the
# first failing test binary, and main's crates/bridge-fuse/tests/owner_turn.rs:295 fails before the client, server and
# CLI crates run. Here every binary runs (--no-fail-fast) with the Test step's environment, then the namespace pair and
# the CLI flow, in ci.yml's order (the workspace before the AppArmor change, the namespace pair after it, the CLI flow
# after user_allow_other), each bounded so that a hang still leaves its log and the summary.
set +e
label=${1:-landing}
R=../results.txt
{
  echo "A-232 landing tests ($label) on $(uname -m), $(nproc) CPUs; kernel $(uname -r)"
  echo "commit $(git rev-parse HEAD), parent $(git rev-parse HEAD~1)"
} > "$R"

# Runs one phase within `minutes` into `log`, then writes its exit code, every failed test with the binary that ran it,
# the targets cargo names as failed, the binary still running if it timed out, and every SKIP line.
phase() {
  local name=$1 minutes=$2 log=$3
  shift 3
  local started=$SECONDS
  timeout -k 60 "${minutes}m" "$@" > "../$log" 2>&1
  local code=$?
  echo "== $name: exit $code in $((SECONDS - started)) s ($log)" | tee -a "$R"
  awk '/^ +Running /{bin=$0; sub(/^ +Running /,"",bin)} /^ +Doc-tests /{bin=$0; sub(/^ +/,"",bin)}
       / \.\.\. FAILED$/{t=$0; sub(/^test /,"",t); sub(/ \.\.\. FAILED$/,"",t); print "   FAILED " t "  [" bin "]"}' \
    "../$log" | tee -a "$R"
  grep -a -E -A60 '^error: [0-9]+ targets? failed:' "../$log" | grep -a -E '^ +`' | sed 's/^ */   target /' | tee -a "$R"
  grep -a -E 'panicked at ' "../$log" | sed -E 's/.*panicked at /   panicked at /' | sort | uniq -c | head -40 | tee -a "$R"
  if [ "$code" -eq 124 ] || [ "$code" -eq 137 ]; then
    echo "   TIMED OUT after ${minutes} min; the last binary started: $(grep -a -E '^ +Running ' "../$log" | tail -1)" | tee -a "$R"
  fi
  grep -a -E 'SKIP' "../$log" | head -20 | sed 's/^/   skip: /' | tee -a "$R"
}

# The Test step of ci.yml's gates job, every binary run.
phase workspace 150 workspace.log env SLATES_FLEET_TRACE=1 SLATES_TEST_HEDGE_STARVATION_SECS=20 \
  SLATES_TEST_RESTART_SLOWDOWN=10 SLATES_TEST_FLEET_DEADLINE_SECS=120 cargo test --workspace --no-fail-fast

# "Clients in namespaces of their own (Linux)": user namespaces unconfined first, as ci.yml does.
if [ -e /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]; then
  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 | tee -a "$R"
fi
phase "namespaces (client netns)" 20 netns.log cargo test -p slates-client --test netns -- --nocapture
phase "namespaces (ipc rendezvous)" 20 rendezvous.log cargo test -p slates-ipc --test rendezvous -- --nocapture \
  --exact a_client_whose_process_the_daemon_cannot_see_is_served

# "Grant user_allow_other (Linux)", then "The slates CLI flow (anchor + daemon)".
grep -qx user_allow_other /etc/fuse.conf 2>/dev/null || echo user_allow_other | sudo tee -a /etc/fuse.conf > /dev/null
echo "user_allow_other in /etc/fuse.conf: $(grep -cx user_allow_other /etc/fuse.conf 2>/dev/null)" | tee -a "$R"
phase "CLI flow" 60 cli.log env SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli --test fs_server -- \
  --test-threads=1 --nocapture
exit 0
