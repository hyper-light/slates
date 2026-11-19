#!/usr/bin/env bash
# Makes the golden vectors of slates-svn's svnadmin verbs (A-628 part 1: load and dump) with stock Subversion's
# svnadmin, over the repository fixtures/dump/full.dump describes (A-593's vectors): each repository here is made by
# `svnadmin create` and a stock `svnadmin load` of that dump, so it holds the same history.
#
# Each case records what svnadmin wrote to stdout and stderr and its exit status, as `<case>.out`, `<case>.err` and
# `<case>.status`; a dump a case writes is `<case>.dump`. The cases:
# - load-full: `svnadmin load` of full.dump into an empty repository (the progress lines);
# - load-deltas: the same from deltas.dump (format 3);
# - load-parts: part1.dump then part2.dump loaded one after the other (`load-parts-1`, `load-parts-2`);
# - load-range: part1.dump, then full.dump with `-r 4:13` (revisions 0 to 3 skipped);
#   each of these four repositories' own `svnadmin dump` is full.dump again, byte for byte: `redumps.txt` holds each
#   one's SHA-256 beside full.dump's, rather than four copies of it;
# - load-parent: `svn mkdir --parents` of sub/dir (revision 1), then full.dump with `--parent-dir sub/dir`: every
#   revision renumbered, copy sources and svn:mergeinfo moved and renumbered; its dump's first revisions
#   (`load-parent-early.dump`, 0 to 4) and, incrementally, the three that renumber and move svn:mergeinfo and a copy
#   (`load-parent-late.dump`, 11 to 13);
# - load-uuid-*: full.dump into a repository whose UUID is set first, with no option, `--ignore-uuid` and
#   `--force-uuid`; each dump's UUID record is what the option decided;
# - dump-r3-6, dump-r3-6-incremental and dump-r5: ranges, the first revision of a range that is not incremental a
#   whole tree in the order `svn_repos_dir_delta2` walks it (kept before revision 7, whose text would double each);
# - dump-quiet: `svnadmin dump -q`, which writes nothing to stderr;
# - errors: `dump -r 20` (past the youngest), `dump -r 5:3`, a second load of full.dump into the loaded repository (its
#   first record conflicts), a load of a cut dump, and a load of a dump whose text checksum is wrong.
#
# Deterministic: a fresh HOME and configuration directory, a fixed UUID on every repository made here, and revision 0's
# svn:date set where a load does not set it. Run twice, it writes the same bytes.
#
# Usage: make-fixtures.sh OUTDIR   (run from the repository's root, as the command below does)
#
# Run on the Mac Studio (adas-mac-studio, macOS 26.4, arm64) on 2026-11-19 with Homebrew's Subversion 1.15.0
# (r1937767), through kiln with sync off, from a clone of the branch that carried this script, exactly:
#   OUT=$(mktemp -d ~/.cache/g264-admin.XXXXXX) && bash crates/svn/fixtures/svnadmin/make-fixtures.sh "$OUT"
set -uo pipefail
OUT=$(cd "$1" && pwd)
DUMPS=$(cd "$(dirname "$0")/../dump" && pwd)
export LC_ALL=C LANG=C TZ=UTC
WORK=$(mktemp -d "$HOME/.cache/g264-adminfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"
mkdir -p "$HOME"
UUID=7bf7a5ef-cabf-0310-b7d4-93df341afa7e
OTHER_UUID=0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0
svn() { command svn --config-dir "$WORK/config" --non-interactive --no-auth-cache --username test "$@"; }
svnadmin --version | head -1 > "$OUT/svn-version.txt"

# Runs `svnadmin ARGS...` as case NAME: stdout, stderr and the exit status recorded.
run() {
  local name=$1
  shift
  svnadmin "$@" > "$OUT/$name.out" 2> "$OUT/$name.err"
  echo $? > "$OUT/$name.status"
}

# Runs `svnadmin load ARGS...` from FILE as case NAME.
load() {
  local name=$1 file=$2
  shift 2
  svnadmin load "$@" < "$file" > "$OUT/$name.out" 2> "$OUT/$name.err"
  echo $? > "$OUT/$name.status"
}

# A fresh repository at WORK/NAME with UUID, revision 0's svn:date fixed.
fresh() {
  local repo="$WORK/$1"
  svnadmin create "$repo"
  svnadmin setuuid "$repo" "${2:-$UUID}"
  printf '2005-04-07T22:13:00.000000Z' > "$WORK/date0"
  svnadmin setrevprop -r 0 "$repo" svn:date "$WORK/date0"
  printf '%s' "$repo"
}

R=$(fresh full)
load load-full "$DUMPS/full.dump" "$R"
D=$(fresh deltas)
load load-deltas "$DUMPS/deltas.dump" "$D"
P=$(fresh parts)
load load-parts-1 "$DUMPS/part1.dump" "$P"
load load-parts-2 "$DUMPS/part2.dump" "$P"
G=$(fresh range)
load load-range-1 "$DUMPS/part1.dump" "$G"
load load-range-2 "$DUMPS/full.dump" -r 4:13 "$G"
for repo in full deltas parts range; do
  printf '%s %s\n' "$repo" "$(svnadmin dump -q "$WORK/$repo" | shasum -a 256 | cut -d' ' -f1)"
done > "$OUT/redumps.txt"
printf 'full.dump %s\n' "$(shasum -a 256 < "$DUMPS/full.dump" | cut -d' ' -f1)" >> "$OUT/redumps.txt"

N=$(fresh parent)
svn mkdir -q --parents -m 'the parent directory' "file://$N/sub/dir"
printf '2005-04-07T22:12:59.000000Z' > "$WORK/date1"
svnadmin setrevprop -r 1 "$N" svn:date "$WORK/date1"
load load-parent "$DUMPS/full.dump" --parent-dir sub/dir "$N"
svnadmin dump -q -r 0:4 "$N" > "$OUT/load-parent-early.dump"
svnadmin dump -q -r 11:13 --incremental "$N" > "$OUT/load-parent-late.dump"

for option in default ignore force; do
  U=$(fresh "uuid-$option" "$OTHER_UUID")
  case $option in
    default) load "load-uuid-$option" "$DUMPS/full.dump" "$U" ;;
    *) load "load-uuid-$option" "$DUMPS/full.dump" "--$option-uuid" "$U" ;;
  esac
  svnadmin dump -q -r 0 "$U" > "$OUT/load-uuid-$option.dump"
done

run dump-r3-6 dump -r 3:6 "$R"
run dump-r3-6-incremental dump -r 3:6 --incremental "$R"
run dump-r5 dump -r 5 "$R"
run dump-quiet dump -q -r 0:2 "$R"
for name in dump-r3-6 dump-r3-6-incremental dump-r5 dump-quiet; do
  mv "$OUT/$name.out" "$OUT/$name.dump"
done

run error-dump-past-youngest dump -r 20 "$R"
run error-dump-backward dump -r 5:3 "$R"
load error-load-again "$DUMPS/full.dump" "$R"
C=$(fresh cut)
head -c 3000 "$DUMPS/full.dump" > "$WORK/cut.dump"
load error-load-cut "$WORK/cut.dump" "$C"
B=$(fresh badsum)
sed -e 's/^Text-content-md5: 6c7831c26f0d0a5f807006854aa682f4$/Text-content-md5: 00000000000000000000000000000001/' \
  "$DUMPS/full.dump" > "$WORK/badsum.dump"
load error-load-badsum "$WORK/badsum.dump" "$B"

cd "$OUT"
shasum -a 256 $(ls | command grep -v '^SHA256SUMS$' | sort) > SHA256SUMS
ls -la "$OUT"
