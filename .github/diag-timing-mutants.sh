#!/bin/bash
# Each mutant is one fault a rewritten test must go red on. Diagnostics only.
set +e
R=../results.txt
echo "mutants, commit $(git rev-parse --short HEAD), $(nproc) CPUs" > $R
run() {
  local name=$1 test=$2 file=$3 old=$4 new=$5
  cp "$file" "$file.orig"
  if ! python3 - "$file" "$old" "$new" <<'PY'
import sys
p,o,n=sys.argv[1:4]
s=open(p).read()
if s.count(o)!=1:
    print("count",s.count(o)); sys.exit(1)
open(p,'w').write(s.replace(o,n))
PY
  then echo "== $name NOT APPLIED" | tee -a $R; mv "$file.orig" "$file"; return; fi
  out=$(cargo test --locked -q -p slates-server --test fleet -- --exact "$test" --nocapture 2>&1); code=$?
  echo "== $name $test exit $code" | tee -a $R
  echo "$out" | grep -E -A3 'panicked|^error' | cut -c1-600 | head -30 | tee -a $R
  mv "$file.orig" "$file"
}
run session_out_gives_up a_forward_waits_for_the_owners_session_while_it_is_out crates/server/src/forward_lane.rs \
$'          count_miss(peer, &mut missed);' \
$'          count_miss(peer, &mut missed);\n          return given_up(peer, &held, begun);'
run location_tried_once a_refuted_false_death_keeps_a_forward_out_for_less_than_its_location_budget crates/server/src/verbs.rs \
$'    if slates_rt::futures::sleep(pace).await.is_err() {\n      return None;\n    }\n    crate::state::with_state(|state| state.count("fleet.owner_location.retried", 1));' \
$'    if pace > 0 {\n      return None;\n    }\n    if slates_rt::futures::sleep(pace).await.is_err() {\n      return None;\n    }\n    crate::state::with_state(|state| state.count("fleet.owner_location.retried", 1));'
run free_read_on_the_held_shard forwards_to_one_owner_never_wait_behind_each_other crates/server/tests/fleet.rs \
$'let fast = volume_owned_by(&mut client_a, 0, "fast", &daemons);' \
$'let fast = volume_owned_by(&mut client_a, 1, "fast", &daemons);'
run hedge_after_the_hold a_slow_first_round_candidate_is_hedged_after_the_measured_p95 crates/server/src/fleet.rs \
$'fn hedge_delay_ns(latency: &LatencyWindow) -> u64 {' \
$'fn hedge_delay_ns(latency: &LatencyWindow) -> u64 {\n  if latency.len() < usize::MAX {\n    return 30_000_000_000;\n  }'
