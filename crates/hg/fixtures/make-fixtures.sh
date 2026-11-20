#!/usr/bin/env bash
# Makes the golden vectors of slates-hg (A-514) with stock Mercurial 7.2, from PyPI through uv (nothing installed
# system-wide). Deterministic: no configuration but this script's, fixed user, every date 0 0 (devel.default-date).
# Usage: make-fixtures.sh OUTDIR   (OUTDIR receives cg01.hg cg02.hg cg03.hg cg04.hg and each one's .debug listing)
#
# Run once to make the fixtures beside this script, on the Mac Studio (adas-mac-studio, macOS 26.4, arm64), 2026-11-19,
# with uv's Mercurial 7.2 from PyPI on CPython 3.12.16, through kiln (sync off), exactly:
#   OUT=$(mktemp -d ~/.cache/g291-fixtures.XXXXXX) && ~/.cache/g291-scratch/make-fixtures.sh "$OUT"
# The ten files' SHA-256 were printed there and checked equal here after the copy.
# 7.2's `debugbundle --all` fails on changegroup 04 (below) and lists no file of a 03 changegroup: both are 7.2's
# own listing's limits, and the tests say which rows each listing holds.
set -euo pipefail
OUT=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
hg() { uvx --quiet --python 3.12 --from 'mercurial==7.2' hg --config ui.username=test \
  --config devel.default-date='0 0' --config experimental.evolution.createmarkers=true \
  --config experimental.evolution.exchange=true "$@"; }
WORK=$(mktemp -d "$HOME/.cache/g291-hgfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"
hg version -q
hg init repo
cd repo
printf 'a\n' > a; hg add -q a; hg commit -m 'add a'
printf 'a\nb\n' > a; printf 'x\n' > b; hg add -q b; hg commit -m 'edit a, add b'
hg branch -q stable; printf 'a\nb\nc\n' > a; hg commit -m 'on stable'
hg update -q default; hg cp b c; hg commit -m 'copy b to c'
hg bookmark -r . book
hg merge -q stable; hg commit -m 'merge stable'
mkdir dir; printf 'deep\n' > dir/f; hg add -q dir/f; hg commit -m 'add dir/f'
printf 'deep\nmore\n' > dir/f; hg commit -m 'draft edit'
printf 'deep\nmore, amended\n' > dir/f; hg commit -q --amend -m 'draft edit, amended'
hg phase -q --public -r 'ancestors(5)'
hg log -T '{rev} {node} {phase} {branch} {p1node} {p2node} {desc|firstline}\n' > "$OUT/log.txt"
for v in 02 03 04; do
  hg bundle -q --all --type "none-v2;cg.version=$v;phases=true;obsolescence=true" "$OUT/cg$v.hg"
done
hg bundle -q --all --type none-v1 "$OUT/cg01.hg"
for v in 01 02 03; do hg debugbundle --all "$OUT/cg$v.hg" > "$OUT/cg$v.hg.debug"; done
# 7.2's `debugbundle --all` fails on changegroup 04 (a full text has no delta: TypeError at debugcommands.py:398),
# so 04 gets the plain listing and its revisions are judged against 03's.
hg debugbundle "$OUT/cg04.hg" > "$OUT/cg04.hg.debug"
for r in 0 1 2 3 4 5 7; do for p in a b c dir/f; do
  if hg cat -q -r "$r" "$p" > /dev/null 2>&1; then printf '%s %s %s\n' "$r" "$p" "$(hg cat -r "$r" "$p" | base64)"; fi
done; done > "$OUT/texts.txt"
ls -la "$OUT"
