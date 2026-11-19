#!/usr/bin/env bash
# Makes the compressed golden vectors of slates-hg (A-598) with stock Mercurial 7.2, from PyPI through uv (nothing
# installed system-wide), over the repository ../make-fixtures.sh builds: its steps are repeated here exactly, so each
# uncompressed twin below is byte for byte ../cg01.hg or ../cg02.hg, which the run checks.
# Usage: make-compressed-fixtures.sh OUTDIR
#   OUTDIR receives cg01-gzip.hg and cg01-bzip2.hg (bundle1: HG10GZ, HG10BZ), cg02-gzip.hg, cg02-bzip2.hg and
#   cg02-zstd.hg (bundle2 with Compression=GZ, BZ and ZS, the spec of ../cg02.hg otherwise), the uncompressed twins
#   cg01-none.hg and cg02-none.hg, and SHA256SUMS of them all. Each engine at 7.2's own default level (zlib -1, bzip2
#   9, zstd 3: utils/compression.py).
#
# Run once to make the fixtures beside this script, on the Mac Studio (Adas-Mac-Studio, macOS 26.4, arm64), 2026-11-19,
# with uv's Mercurial 7.2 from PyPI on CPython 3.12 (its engines: bz2, bz2truncated, none, zlib, zstd), through kiln
# (sync off), from a clone of this branch, exactly:
#   OUT=$(mktemp -d ~/.cache/g079-a598-fixtures.XXXXXX) && crates/hg/fixtures/compressed/make-compressed-fixtures.sh "$OUT"
# The run found cg01-none.hg and cg02-none.hg equal to ../cg01.hg and ../cg02.hg byte for byte (cmp). The five
# compressed files' SHA-256, printed there, are SHA256SUMS beside this script, checked equal here after the copy.
set -euo pipefail
OUT=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
hg() { uvx --quiet --python 3.12 --from 'mercurial==7.2' hg --config ui.username=test \
  --config devel.default-date='0 0' --config experimental.evolution.createmarkers=true \
  --config experimental.evolution.exchange=true "$@"; }
WORK=$(mktemp -d "$HOME/.cache/g079-hgfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"
hg version -q
hg debuginstall 2>&1 | grep -i 'compression engines' || true
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
for engine in gzip bzip2 none; do
  hg bundle -q --all --type "$engine-v1" "$OUT/cg01-$engine.hg"
done
for engine in gzip bzip2 zstd none; do
  hg bundle -q --all --type "$engine-v2;cg.version=02;phases=true;obsolescence=true" "$OUT/cg02-$engine.hg"
done
cd "$OUT"
shasum -a 256 cg01-*.hg cg02-*.hg > SHA256SUMS
cat SHA256SUMS
