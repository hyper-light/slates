#!/bin/bash
# measure.sh A_DIR B_DIR N BACKLOG OUT: release builds of both trees, then N runs each of the catch-up flow, ABBA order.
A=$1; B=$2; N=$3; BACKLOG=$4; OUT=$5
TA=${TARGET_BASE:-$HOME/.cache/g227-measure}/a; TB=${TARGET_BASE:-$HOME/.cache/g227-measure}/b
export SLATES_TEST_CLI=1 SLATES_CATCH_UP_BACKLOG=$BACKLOG CARGO_TERM_COLOR=never
echo "host $(hostname) $(uname -sm), $(getconf _NPROCESSORS_ONLN) CPUs, $(date -u), A $(git -C $A rev-parse --short HEAD) (main + flow), B $(git -C $B rev-parse --short HEAD) (pair reverted), backlog $BACKLOG, N $N" > $OUT
for leg in a b; do
  dir=$A; t=$TA; [ $leg = b ] && { dir=$B; t=$TB; }
  ( cd $dir && CARGO_TARGET_DIR=$t cargo build --release --locked -p slates-cli --bin slates --tests > $OUT.build-$leg 2>&1 ); echo "== build $leg exit $?" >> $OUT
done
order=""; for i in $(seq 1 $N); do if [ $((i % 2)) = 1 ]; then order="$order a b"; else order="$order b a"; fi; done
run=0; TO=$(command -v timeout || command -v gtimeout); TO=${TO:+$TO 1200}
for leg in $order; do
  run=$((run+1)); dir=$A; t=$TA; [ $leg = b ] && { dir=$B; t=$TB; }
  load=$(uptime | sed 's/.*load averages*: //')
  ( cd $dir && CARGO_TARGET_DIR=$t $TO cargo test --release --locked -p slates-cli --test live_chaos -- --nocapture --test-threads=1 --exact a_resumed_holders_catch_up_beside_a_busy_volume ) > $OUT.run 2>&1; status=$?
  echo "== run $run leg $leg exit $status load $load" >> $OUT
  grep -E '^LD-CU |panicked' $OUT.run | cut -c1-600 | sed "s/^/   $run$leg /" >> $OUT
done
echo "== done $(date -u)" >> $OUT
