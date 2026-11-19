#!/usr/bin/env bash
# Makes the golden vectors of slates-svn's dump codec (A-593) with stock Subversion: one small repository built by the
# stock client over file://, then dumped by stock svnadmin four ways: whole in format 2 (`full.dump`), whole in format 3
# (`deltas.dump`, `--deltas`), and in two parts whose concatenation is one stream holding two version records
# (`part1.dump`, revisions 0 to 3 in format 2; `part2.dump`, revisions 4 on, `--deltas --incremental`, format 3).
# `changed.txt` is `svnlook changed --copy-info` for every revision, the stock tool's own account of what each one did;
# `svn-version.txt` names the svnadmin that wrote the dumps; `SHA256SUMS` holds every file's SHA-256, which the tests
# check before they read a vector.
#
# What the history holds, each a case the codec must carry: texts of every shape (lines, binary bytes with NUL and no
# final newline, empty, a symbolic link, an executable, a path holding ": " and spaces, a text past one svndiff window
# changed in both windows); node properties set, changed and deleted, an empty value, a binary value, a multi-line value,
# properties of the root (an empty Node-path); a directory copied, a file copied as it was and copied then changed, a
# copy made from an older revision; a file and a directory deleted; a file and a directory replaced without history and
# replaced by a copy (svnadmin writes that as a delete record and an add record), one of them changed after the copy;
# a server-side tag; a merge, which sets svn:mergeinfo; revision properties beyond the three, a multi-line log and an
# empty one.
#
# Deterministic: no configuration but this script's (a fresh HOME and configuration directory), a fixed UUID (the one
# notes/dump-load-format.txt shows), a fixed author and messages, and every revision's svn:date set to a fixed instant
# once the commits are made. Run twice, it writes the same bytes.
#
# Usage: make-fixtures.sh OUTDIR
#
# Run on the Mac Studio (adas-mac-studio, macOS 26.4, arm64) on 2026-11-19 with Homebrew's Subversion 1.15.0
# (r1937767), whose libsvn_repos dump.c and libsvn_subr hash.c are byte for byte those of the 1.14.x branch at
# d434dc14 that docs/wip/research/vcs-servers-subversion.md reads (§11 there), through kiln with sync off, from a clone
# of the branch that carried this script, exactly:
#   OUT=$(mktemp -d ~/.cache/g264-fixtures.XXXXXX) && bash crates/svn/fixtures/dump/make-fixtures.sh "$OUT"
set -euo pipefail
OUT=$(cd "$1" && pwd)
export LC_ALL=C LANG=C TZ=UTC
WORK=$(mktemp -d "$HOME/.cache/g264-svnfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"
mkdir -p "$HOME"
svn() { command svn --config-dir "$WORK/config" --non-interactive --no-auth-cache --username test "$@"; }
svnadmin --version | head -1 > "$OUT/svn-version.txt"
REPO="$WORK/repo"
URL="file://$REPO"
svnadmin create "$REPO"
svnadmin setuuid "$REPO" 7bf7a5ef-cabf-0310-b7d4-93df341afa7e
svn checkout -q "$URL" "$WORK/wc"
cd "$WORK/wc"

# r1: the layout and a text of every shape, with properties.
mkdir -p trunk/dir branches tags
printf 'alpha\nbeta\ngamma\n' > trunk/a.txt
printf 'line\r\nwith CRLF, a NUL \0 and \377, and no final newline' > trunk/bin.dat
: > trunk/empty
ln -s a.txt trunk/link
printf '#!/bin/sh\necho run\n' > trunk/run.sh
chmod +x trunk/run.sh
printf 'deep\n' > trunk/dir/deep.txt
printf 'spaced\n' > 'trunk/a: b c.txt'
svn add -q trunk branches tags
svn propset -q svn:keywords Id trunk/a.txt
svn propset -q empty '' trunk/a.txt
svn propset -q custom 'a value' trunk/dir
svn commit -q -m 'r1: the layout, texts of each shape, properties'
svn update -q

# r2: a text changed; properties changed, deleted, and set on a directory, on the root and with a binary value.
printf 'alpha\nBETA\ngamma\ndelta\n' > trunk/a.txt
svn propset -q svn:keywords 'Id Rev' trunk/a.txt
svn propdel -q empty trunk/a.txt
printf '*.o\n*.tmp\n' > "$WORK/ignores"
svn propset -q svn:ignore -F "$WORK/ignores" trunk
printf 'binary\0value' > "$WORK/binary-value"
svn propset -q binary -F "$WORK/binary-value" trunk/bin.dat
svn propset -q root-prop 'on the root' .
svn commit -q -m "$(printf 'r2: texts and properties changed\n\nA second paragraph.')"
svn update -q

# r3: a directory copied, a file copied then changed, a file copied as it was with a property of its own.
svn copy -q trunk branches/b1
svn copy -q trunk/a.txt trunk/a-copy.txt
printf 'copied, then changed\n' >> trunk/a-copy.txt
svn copy -q trunk/empty trunk/empty-copy
svn propset -q copied-prop yes trunk/empty-copy
svn commit -q -m 'r3: a branch, a file copied then changed, a file copied as it was'
svn update -q

# r4: a file and a directory deleted.
svn delete -q trunk/link trunk/dir
svn commit -q -m 'r4: a file and a directory deleted'
svn update -q

# r5: a file and a directory replaced without history.
svn delete -q trunk/run.sh
printf '#!/bin/sh\necho replaced\n' > trunk/run.sh
svn add -q trunk/run.sh
svn delete -q branches/b1/dir
svn mkdir -q branches/b1/dir
svn commit -q -m 'r5: a file and a directory replaced without history'
svn update -q

# r6: a file and a directory replaced by copies, and a file replaced by a copy and then changed.
svn delete -q trunk/bin.dat
svn copy -q trunk/a.txt trunk/bin.dat
svn delete -q branches/b1
svn copy -q trunk branches/b1
svn delete -q 'trunk/a: b c.txt'
svn copy -q trunk/a.txt 'trunk/a: b c.txt'
printf 'and changed\n' >> 'trunk/a: b c.txt'
svn commit -q -m 'r6: a file and a directory replaced by copies'
svn update -q

# r7 and r8: a text past one svndiff window (102,400 bytes), then changed in its first window and its second.
for line in $(seq 1 1750); do
  printf 'line %05d of a text long enough to span two svndiff windows\n' "$line"
done > trunk/big.txt
svn add -q trunk/big.txt
svn commit -q -m 'r7: a text past one svndiff window'
sed -e 's/^line 00010 /line 00010 changed /' -e 's/^line 01740 /line 01740 changed /' trunk/big.txt > "$WORK/big"
mv "$WORK/big" trunk/big.txt
svn commit -q -m 'r8: that text changed in both windows'
svn update -q

# r9: a tag, copied on the server.
svn copy -q -m 'r9: a tag' "$URL/trunk" "$URL/tags/v1"
svn update -q

# r10 and r11: a change on the branch, then the branch merged to trunk, which sets svn:mergeinfo.
printf 'a change on the branch\n' >> branches/b1/a.txt
svn commit -q -m 'r10: a change on the branch'
svn update -q
svn merge -q "$URL/branches/b1" trunk
svn commit -q -m 'r11: the branch merged to trunk'
svn update -q

# r12: a copy from an older revision, with an empty log message.
svn copy -q -m '' "$URL/trunk/a.txt@1" "$URL/trunk/a-from-r1.txt"
svn update -q

# r13: a property deleted from the root.
svn propdel -q root-prop .
svn commit -q -m 'r13: a property deleted from the root'
svn update -q

# Revision properties: one beyond the three, and every svn:date fixed.
printf 'a revision property of its own' > "$WORK/revprop"
svnadmin setrevprop -r 2 "$REPO" my:revprop "$WORK/revprop"
YOUNGEST=$(svnlook youngest "$REPO")
for revision in $(seq 0 "$YOUNGEST"); do
  printf '2005-04-07T22:13:%02d.000000Z' "$revision" > "$WORK/date"
  svnadmin setrevprop -r "$revision" "$REPO" svn:date "$WORK/date"
done

svnadmin dump -q "$REPO" > "$OUT/full.dump"
svnadmin dump -q --deltas "$REPO" > "$OUT/deltas.dump"
svnadmin dump -q -r 0:3 "$REPO" > "$OUT/part1.dump"
svnadmin dump -q --deltas --incremental -r "4:$YOUNGEST" "$REPO" > "$OUT/part2.dump"
for revision in $(seq 1 "$YOUNGEST"); do
  printf 'r%s\n' "$revision"
  svnlook changed --copy-info -r "$revision" "$REPO"
done > "$OUT/changed.txt"
cd "$OUT"
shasum -a 256 full.dump deltas.dump part1.dump part2.dump changed.txt svn-version.txt > SHA256SUMS
ls -la "$OUT"
cat SHA256SUMS
