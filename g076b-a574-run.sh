source ~/.cargo/env 2>/dev/null; set -u
W="$HOME/.cache/g076b-a574"; mkdir -p "$W" && cd "$W" || exit 9
export CARGO_TARGET_DIR="$W/target"
uptime; date -u
run() { echo "=== cargo test --locked -p $1 --lib -- ${*:2}"; cargo test --locked -p "$1" --lib -- "${@:2}" 2>&1 | command grep -E "^test |test result|panicked|left:|right:|FAILED|^error" | tail -40; echo "=== exit ${PIPESTATUS[0]} ($1)"; }
echo "##### RED (agents/g076b-a574-red)"
rm -rf src-red && git clone -q --depth 1 -b agents/g076b-a574-red https://github.com/hyper-light/slates.git src-red && cd src-red || exit 9
git log --oneline -1 | cut -c1-80
run slates-cluster failover::
run slates-server laptop_failover_tests
cd "$W" || exit 9
echo "##### GREEN (agents/g076b-a574)"
rm -rf src && git clone -q --depth 1 -b agents/g076b-a574 https://github.com/hyper-light/slates.git src && cd src || exit 9
git log --oneline -1 | cut -c1-80
run slates-cluster failover::
run slates-anchor ""
run slates-server laptop_failover_tests fleet::tests::a_shard_is_fanned fleet::tests::an_installed_fan watchdog late_beat_tests starved_share_tests
run slates-api role::anchor
echo "=== cargo fmt --all --check"; cargo fmt --all --check > "$W/fmt.txt" 2>&1; echo "=== exit $? (fmt)"; head -20 "$W/fmt.txt"
date -u
