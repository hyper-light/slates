#!/usr/bin/env bash
# A-680 part 1 run on the Mac Studio (macOS arm64): red (the view's filter and its in-place lending stubbed out in the
# clone), then green (slates-hg serve:: and manifest::, slates-server vcs_http::hg at 4 threads), then fmt. One target
# dir kept for reuse while the piece is in flight; the clone deleted at the end; logs kept in ~/.cache/g303-logs.
set -uo pipefail
export CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=$HOME/.cache/g303-target
LOGS=$HOME/.cache/g303-logs; mkdir -p "$LOGS"
CLONE=$HOME/.cache/g303-a680-clone
BRANCH=${BRANCH:-agents/g303-a680}
rm -rf "$CLONE"
git clone -q --depth 1 -b "$BRANCH" https://github.com/hyper-light/slates.git "$CLONE" || exit 2
cd "$CLONE" || exit 2
echo "== tree $(git log --oneline -1 | cut -c1-90)"
if [ "${SKIP_RED:-0}" != 1 ]; then
python3 - <<'PY' || exit 3
import pathlib
p = pathlib.Path('crates/hg/src/serve/indexed.rs')
s = p.read_text()
cuts = [
  ("    if self.candidate && !self.reached {", "    if false && self.candidate && !self.reached {"),
  ("    (self.phase > Phase::Draft).then_some(LeftOut::Secret)", "    (false && self.phase > Phase::Draft).then_some(LeftOut::Secret)"),
  ("  if windows.next().is_none() {", "  if false && windows.next().is_none() {"),
]
for old, new in cuts:
    assert s.count(old) == 1, old
    s = s.replace(old, new)
p.write_text(s)
print("red stub applied: nothing left out, nothing lent in place")
PY
cargo test --locked -p slates-hg --lib -- serve::indexed > "$LOGS/red-hg.log" 2>&1; echo "== red-hg exit $?"
grep -E '^test .* (ok|FAILED)$|^test result' "$LOGS/red-hg.log" | tail -20
git show HEAD:crates/hg/src/serve/indexed.rs > crates/hg/src/serve/indexed.rs
echo "== restored: $(git status --short | wc -l | tr -d ' ') files differ from HEAD"
fi
cargo test --locked -p slates-hg --lib -- serve:: manifest:: > "$LOGS/green-hg.log" 2>&1; echo "== green-hg exit $?"
grep -E 'Compiling slates-hg|^test result|FAILED|panicked' "$LOGS/green-hg.log" | tail -20
cargo test --locked -p slates-server --lib vcs_http::hg -- --test-threads=4 > "$LOGS/green-server.log" 2>&1; echo "== green-server exit $?"
grep -E 'Compiling slates-server|^test result|FAILED|panicked|^error' "$LOGS/green-server.log" | tail -20
cargo fmt --check -p slates-hg -p slates-server > "$LOGS/fmt.log" 2>&1; echo "== fmt exit $?"; tail -5 "$LOGS/fmt.log"
cd "$HOME" && rm -rf "$CLONE"
echo "== done; logs in $LOGS"
