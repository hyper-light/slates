#!/bin/sh
# Builds what run.js (beside this file) runs on one machine of the two-machine flow: the `slates` CLI and the
# `fs_server` test binary, whose roles are the per-machine steps, from one pushed branch, in a clone and a target
# directory of their own under ~/.cache (never a synced or shared tree).
#
#   sh docs/wip/bench/fs-two-machines/build.sh BRANCH
#
# Run it on both machines of a run at the same branch, so both nodes speak one commit's protocol; on a machine whose
# house rules cap every command (adacachy), run it inside the same `systemd-run --user` caps as run.js. Prints
# COMMIT=, BUILD_S=, SLATES= and TESTBIN=, the last two run.js's `sb` or `cb`.
set -u
branch=${1:?a pushed branch of hyper-light/slates}
src=$HOME/.cache/fs-two-src
export CARGO_TARGET_DIR="$HOME/.cache/slates-work-target-fs-two" CARGO_TERM_COLOR=never
log=$HOME/.cache/fs-two-build.log
rm -rf "$src"
git clone -q --depth 1 -b "$branch" https://github.com/hyper-light/slates.git "$src" || exit 10
cd "$src" || exit 10
echo "COMMIT=$(git rev-parse HEAD)"
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
start=$(date +%s)
cargo build --locked -p slates-cli --bin slates > "$log" 2>&1 || { tail -40 "$log"; exit 11; }
cargo test --locked -p slates-cli --test fs_server --no-run >> "$log" 2>&1 || { tail -40 "$log"; exit 12; }
testbin=$(sed -nE 's/.*Executable tests\/fs_server\.rs \((.*)\)/\1/p' "$log" | tail -1)
case $testbin in /*) ;; *) testbin=$src/$testbin ;; esac
echo "BUILD_S=$(( $(date +%s) - start ))"
echo "SLATES=$CARGO_TARGET_DIR/debug/slates"
echo "TESTBIN=$testbin"
