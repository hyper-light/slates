#!/usr/bin/env bash
# A-680 part 2b landing run on the Mac Studio (macOS arm64): focused hg + server unit tests, fmt, then the stock-hg
# HTTP round trip (ignored on the branch) run with --ignored to see whether it can be un-ignored.
set -uo pipefail
export CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=$HOME/.cache/g303-target
LOGS=$HOME/.cache/w-hg-logs; mkdir -p "$LOGS"
CLONE=$HOME/.cache/w-hg-clone
BRANCH=${BRANCH:-agents/g303-a680-p2b-land2}
rm -rf "$CLONE"
git clone -q --depth 1 -b "$BRANCH" https://github.com/hyper-light/slates.git "$CLONE" || exit 2
cd "$CLONE" || exit 2
echo "== tree $(git log --oneline -1 | cut -c1-90)"
if [ "${ONLY_CLI:-0}" != 1 ]; then
cargo test --locked -p slates-hg --lib -- push:: serve:: > "$LOGS/hg.log" 2>&1; echo "== hg exit $?"
grep -E '^test result|FAILED|panicked|^error' "$LOGS/hg.log" | tail -20
cargo test --locked -p slates-server --lib -- vcs_http:: vcs_store::hg_view repository::hg_push vcs_ssh::hg --test-threads=4 > "$LOGS/server.log" 2>&1; echo "== server exit $?"
grep -E '^test result|FAILED|panicked|^error|^warning' "$LOGS/server.log" | tail -20
cargo fmt --check -p slates-hg -p slates-server -p slates-cli > "$LOGS/fmt.log" 2>&1; echo "== fmt exit $?"; tail -5 "$LOGS/fmt.log"
fi
if [ "${SKIP_CLI:-0}" != 1 ]; then
export SLATES_TEST_HG="uvx --quiet --python 3.12 --from mercurial==7.2 hg"
$SLATES_TEST_HG --version | head -1
cargo test --locked -p slates-cli --test cli -- --exact stock_hg_clones_pulls_and_pushes_over_the_vcs_http_edge stock_hg_without_credentials_is_refused_in_its_own_words --include-ignored --test-threads=1 --nocapture > "$LOGS/cli.log" 2>&1; echo "== cli exit $?"
grep -E '^test |^test result|FAILED|panicked|SKIP|skipping|abort' "$LOGS/cli.log" | tail -30
fi
cd "$HOME" && rm -rf "$CLONE"
echo "== done; logs in $LOGS"
