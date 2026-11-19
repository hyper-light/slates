#!/usr/bin/env bash
# Makes slates-hg's object golden vectors (A-588) with stock Mercurial 7.2, from PyPI through uv (nothing installed
# system-wide). Deterministic: no configuration but this script's, fixed users and dates. Usage: make-object-fixtures.sh OUT
# One repository whose history holds every shape an object takes: flags `x` and `l`, a copy (its metadata block), a
# file whose bytes begin as the block does (an empty block before them), a named branch (an empty file list and the
# `branch` extra), a date with a timezone, a description of several lines, a merge, a closed branch head (`close`), a
# user beyond ASCII, and extras holding every byte 7.2 escapes (written through 7.2's own `repo.commit`). Then 7.2's own
# debug commands write each log:
#   index-changelog.txt, index-manifest.txt, index-file-<path>.txt   `hg debugindex --debug`: a header naming the
#       columns, then every revision's number, rank, link, node, parents, full size, delta base and flags;
#   changeset-<rev>.txt, manifest-<rev>.txt, file-<path>-<rev>.txt   `hg debugdata`: every revision's stored text, a
#       file revision's metadata block included;
#   expected.txt   what 7.2 reads from each changeset (`changectx`), each manifest entry and each file revision's copy
#       (`filelog.renamed`), every value hex-encoded;
#   log.txt        the graph.
# Last it prints the SHA-1 7.2's `hashutil` uses (sha1dc's when its C extension is built) and every file's SHA-256.
set -euo pipefail
OUT=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
WORK=$(mktemp -d "$HOME/.cache/g291-objfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
hg() { uvx --quiet --python 3.12 --from mercurial==7.2 hg --config ui.username='Test <test@example.com>' \
  --config devel.default-date='0 0' "$@"; }
py() { uvx --quiet --python 3.12 --from mercurial==7.2 python -I "$@"; }
cat > "$WORK/extras.py" <<'PY'
import sys
from mercurial import hg, ui as uimod
repo = hg.repository(uimod.ui.load(), sys.argv[1].encode())
with repo.wlock(), repo.lock():
    repo.commit(text=b"escaped extras", user=b"Test <test@example.com>", date=b"0 0",
                extra={b"note": b"a\nb\x00c\\d\re\x0012:tail", b"source": b"svn:1"})
PY
cat > "$WORK/expected.py" <<'PY'
import os, sys
from mercurial import hg, ui as uimod
repo = hg.repository(uimod.ui.load(), sys.argv[1].encode()).unfiltered()
out = sys.argv[2]
expected = []
def h(value):
    return value.hex() if value else "-"
paths = sorted({f for rev in repo for f in repo[rev].files()})
for rev in repo:
    ctx = repo[rev]
    when, offset = ctx.date()
    extra = ",".join("%s:%s" % (k.hex(), v.hex()) for k, v in sorted(ctx.extra().items()))
    expected.append(" ".join(["changeset", str(rev), h(ctx.manifestnode()), h(ctx.user()), "%d" % when, "%d" % offset,
                              h(ctx.description()), ",".join(f.hex() for f in ctx.files()) or "-", extra or "-"]))
    manifest = ctx.manifest()
    for path in sorted(manifest):
        expected.append(" ".join(["entry", str(rev), path.hex(), manifest[path].hex(), manifest.flags(path).decode() or "-"]))
for path in paths:
    log = repo.file(path)
    for rev in range(len(log)):
        renamed = log.renamed(log.node(rev))
        copy = "%s %s" % (renamed[0].hex(), renamed[1].hex()) if renamed else "- -"
        expected.append(" ".join(["filerev", path.hex(), str(rev), copy]))
open(os.path.join(out, "expected.txt"), "w").write("\n".join(expected) + "\n")
PY
# The rows of a `debugindex` listing: its lines after the header.
rows() { tail -n +2 "$1" | wc -l | tr -d ' '; }
# `debugdata` of every revision a listing holds: dump LISTING PREFIX ARGUMENT...
dump() {
  local listing=$1 prefix=$2 rev=0 count
  shift 2
  count=$(rows "$listing")
  while [ "$rev" -lt "$count" ]; do
    hg debugdata "$@" "$rev" > "$OUT/$prefix-$rev.txt"
    rev=$((rev + 1))
  done
}
cd "$WORK"
hg version -q
hg init repo
cd repo
printf 'a\n' > a; hg add -q a; hg commit -m 'add a'
printf 'a\nb\n' > a; printf '#!/bin/sh\n' > run; chmod +x run; ln -s a link; hg add -q run link
hg commit -m 'an executable and a link'
hg cp -q a c; hg commit -m 'copy a to c'
printf '\001\nbegins as the block does\n' > meta; hg add -q meta; hg commit -m 'a file that begins as the block does'
hg branch -q stable; hg commit -m 'open stable'
printf 'c2\n' > c; hg commit -m 'on stable' -d '1234567890 -3600'
hg update -q default; printf 'd\n' > d; hg add -q d
printf 'several\nlines\n\nof description' > "$WORK/message"; hg commit -l "$WORK/message"
hg merge -q stable; hg commit -m 'merge stable'
hg update -q stable; hg commit --close-branch -m 'close stable'
hg update -q default; printf 'u\n' > u; hg add -q u; hg commit -u "$(printf 'T\303\251st <t@example.com>')" -m 'a user beyond ASCII'
printf 'e\n' > e; hg add -q e
py "$WORK/extras.py" "$WORK/repo"
hg debugindex --debug -c > "$OUT/index-changelog.txt"
dump "$OUT/index-changelog.txt" changeset -c
hg debugindex --debug -m > "$OUT/index-manifest.txt"
dump "$OUT/index-manifest.txt" manifest -m
hg manifest --all > "$WORK/paths"
while IFS= read -r path; do
  name=${path//\//_}
  hg debugindex --debug "$path" > "$OUT/index-file-$name.txt"
  dump "$OUT/index-file-$name.txt" "file-$name" "$path"
done < "$WORK/paths"
py "$WORK/expected.py" "$WORK/repo" "$OUT"
hg log -G -T '{rev} {node|short} {branch} {desc|firstline}\n' > "$OUT/log.txt"
py -c 'from mercurial.utils import hashutil; print("hashutil.sha1 is", hashutil.sha1)'
cd "$OUT"
shasum -a 256 -- *
