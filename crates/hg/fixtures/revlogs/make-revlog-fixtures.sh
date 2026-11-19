#!/usr/bin/env bash
# Makes slates-hg's revlog golden vectors (A-611) with stock Mercurial 7.2, from PyPI through uv (nothing installed
# system-wide). Fixed users and dates; the pseudo-random file is SHA-256 in counter mode, so every text is the same on
# every run (revlog v2's file names carry random ids, which the copies here drop). Usage: make-revlog-fixtures.sh OUT
# One history, made in five repositories: `zstd` (7.2's default revlog compression), `zlib`
# (`format.revlog-compression=zlib`), `split` (zstd, with an extension lowering 7.2's `revlog._maxinline` to 256 bytes,
# so every revlog keeps its data in a `.d` file beside its index), `v2` (`experimental.revlogv2`: a docket, a `.idx`
# index of 96-byte entries and a `.dat` data file) and `clv2` (also `format.exp-use-changelog-v2`). Every repository
# uses generaldelta and sparse-revlog (7.2's defaults) with `format.maxchainlen=3`, so chains end at snapshots, and
# `storage.fileindex.slow-path=allow`, since revlog v2 brings a `fileindex` store whose Rust implementation the PyPI
# build lacks (7.2 aborts without it: "accessing `fileindex` repository without associated fast implementation"). The
# history: a file of 96 lines of SHA-256 hexadecimal that grows a line a changeset (large enough, and changed little
# enough, that sparse-revlog writes intermediate snapshots), a pseudo-random file (stored raw), an empty file, a second
# head and a merge.
# For each repository R and each revlog L (`00changelog`, `00manifest`, the files `grow`, `rand`, `empty`):
#   R-<store path with / as _>   the store's revlog files (`.i`, `.d`; for v2 the docket `.i` and its `.idx`, `.dat`
#                                and `.sda`, the random id dropped from their names)
#   R-<L>.index   `hg debugindex --debug` (every column);   R-<L>.chain   `hg debugdeltachain`
#   <L>-<rev>.txt   `hg debugdata` of every revision, once: every repository holds the same history, and each one's
#                   texts are compared with these byte for byte (`cmp`, the script stops on a difference)
#   R-requires.txt   the store's requirements
# Last it prints every file's SHA-256.
#
# Run on the Mac Studio (macOS 26.4, arm64), 2026-11-19, with uv's Mercurial 7.2 from PyPI on CPython 3.12, through
# kiln (sync off), from a clone of the branch, exactly:
#   D=$(mktemp -d ~/.cache/g291-a611.XXXXXX) && mkdir "$D/out" && bash crates/hg/fixtures/revlogs/make-revlog-fixtures.sh "$D/out"
# Its first run (816910c28) stopped at the v2 repository, which needs `storage.fileindex.slow-path=allow`; its second
# (d92b1f39d) made 300-byte texts for which 7.2 wrote full snapshots only; its third (59bbf094, 04:32 UTC) made these
# 158 files, every SHA-256 printed there and checked equal here after the copy.
set -euo pipefail
OUT=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
WORK=$(mktemp -d "$HOME/.cache/g291-revfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
EXTRA=()
hg() { uvx --quiet --python 3.12 --from mercurial==7.2 hg --config ui.username='Test <test@example.com>' \
  --config devel.default-date='0 0' --config format.maxchainlen=3 --config storage.fileindex.slow-path=allow \
  ${EXTRA[@]+"${EXTRA[@]}"} "$@"; }
py() { uvx --quiet --python 3.12 --from mercurial==7.2 python -I "$@"; }
cat > "$WORK/tinyinline.py" <<'PY'
from mercurial import revlog
revlog._maxinline = 256
PY
cat > "$WORK/random.py" <<'PY'
import hashlib, sys
seed, size = sys.argv[1].encode(), int(sys.argv[2])
out = b"".join(hashlib.sha256(seed + b"%d" % i).digest() for i in range(size // 32 + 1))
sys.stdout.buffer.write(out[:size])
PY
cat > "$WORK/lines.py" <<'PY'
import hashlib, sys
seed, first, last = sys.argv[1].encode(), int(sys.argv[2]), int(sys.argv[3])
for i in range(first, last + 1):
    sys.stdout.write(hashlib.sha256(seed + b"%d" % i).hexdigest() + "\n")
PY
cat > "$WORK/copy.py" <<'PY'
import os, re, shutil, sys
store, out, repo = sys.argv[1], sys.argv[2], sys.argv[3]
for root, _, files in os.walk(store):
    for name in sorted(files):
        if not re.search(r"\.(i|d|idx|dat|sda)$", name):
            continue
        rel = os.path.relpath(os.path.join(root, name), store)
        rel = re.sub(r"-[0-9a-f]+\.(idx|dat|sda)$", r".\1", rel)
        shutil.copyfile(os.path.join(root, name), os.path.join(out, repo + "-" + rel.replace("/", "_")))
PY
history() {
  py "$WORK/lines.py" grow 1 96 > grow; py "$WORK/random.py" first 600 > rand; : > empty
  hg add -q grow rand empty; hg commit -q -m 'add grow, rand and empty'
  for n in 2 3 4 5 6 7; do
    py "$WORK/lines.py" grow "$((95 + n))" "$((95 + n))" >> grow; hg commit -q -m "grow $n"
  done
  py "$WORK/random.py" second 700 > rand; hg commit -q -m 'new random bytes'
  hg update -q 3; py "$WORK/lines.py" head 1 1 >> grow; hg commit -q -m 'second head'
  hg merge -q --tool :local 7; py "$WORK/lines.py" grow 1 102 > grow; py "$WORK/lines.py" head 1 1 >> grow
  hg commit -q -m 'merge the heads'
  for n in 11 12 13; do py "$WORK/lines.py" after "$n" "$n" >> grow; hg commit -q -m "grow $n"; done
}
dump() {
  local repo=$1 name=$2 rev=0 count
  shift 2
  (cd "$WORK/$repo" && hg debugindex --debug "$@") > "$OUT/$repo-$name.index"
  (cd "$WORK/$repo" && hg debugdeltachain "$@") > "$OUT/$repo-$name.chain"
  count=$(($(wc -l < "$OUT/$repo-$name.index") - 1))
  while [ "$rev" -lt "$count" ]; do
    (cd "$WORK/$repo" && hg debugdata "$@" "$rev") > "$WORK/text"
    if [ "$repo" = zstd ]; then cp "$WORK/text" "$OUT/$name-$rev.txt"; else cmp "$WORK/text" "$OUT/$name-$rev.txt"; fi
    rev=$((rev + 1))
  done
}
hg version -q
for repo in zstd zlib split v2 clv2; do
  case $repo in
    zstd) EXTRA=() ;;
    zlib) EXTRA=(--config format.revlog-compression=zlib) ;;
    split) EXTRA=(--config "extensions.tinyinline=$WORK/tinyinline.py") ;;
    v2) EXTRA=(--config experimental.revlogv2=enable-unstable-format-and-corrupt-my-data) ;;
    clv2) EXTRA=(--config format.exp-use-changelog-v2=enable-unstable-format-and-corrupt-my-data
      --config experimental.revlogv2=enable-unstable-format-and-corrupt-my-data) ;;
  esac
  hg init "$WORK/$repo"
  (cd "$WORK/$repo" && history)
  EXTRA=()
  py "$WORK/copy.py" "$WORK/$repo/.hg/store" "$OUT" "$repo"
  cp "$WORK/$repo/.hg/store/requires" "$OUT/$repo-requires.txt" 2>/dev/null || cp "$WORK/$repo/.hg/requires" "$OUT/$repo-requires.txt"
  dump "$repo" 00changelog -c
  dump "$repo" 00manifest -m
  for path in grow rand empty; do dump "$repo" "$path" "$path"; done
done
(cd "$WORK/zstd" && hg log -G -T '{rev} {node|short} {desc|firstline}\n') > "$OUT/log.txt"
cd "$OUT"
shasum -a 256 -- *
