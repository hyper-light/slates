#!/usr/bin/env bash
# Makes the pack index golden vectors of slates-git that `make-fixtures.sh` does not (A-591), with stock git:
# 1. `ofs-large.idx`: the index of `ofs.pack` with its table of 8-byte offsets in use. Stock git puts an offset there
#    only past 2^31 - 1, so no small pack has one; `git index-pack --index-version=2,<offset>` is git's own switch for
#    the table ("to force 64-bit index entries on objects located above the given offset", git's t5302 uses it), and
#    with 0x200 it puts eight of the pack's twelve objects there. Its `git verify-pack -v` listing shows git reads it.
#    Before it, the index git makes of `ofs.pack` without the switch is compared with `ofs.idx`, byte for byte.
# 2. `sha256.pack`, `sha256.idx` and `sha256.verify.txt`: a SHA-256 repository's pack (offset deltas), its index and
#    git's listing, the repository made as `make-fixtures.sh` makes its own.
# Deterministic, as `make-fixtures.sh`: no configuration but this script's, a fixed identity, every date
# 2005-04-07T22:13:13Z. Usage: make-index-fixtures.sh OUTDIR, where OUTDIR holds `make-fixtures.sh`'s `ofs.pack` and
# `ofs.idx`. Run on 2026-11-19 with git 2.50.1 (Apple Git-155); the same day its `index-pack` steps were repeated by
# hand with the same git on another Mac, each index made again byte for byte, and `git verify-pack -v` listed
# `ofs-large.idx` and `sha256.idx` again exactly as recorded.
#
# Sources: git's Documentation/gitformat-pack.txt (idx v2: "large offsets are encoded as an index into the next table
# with the msbit set"; "in SHA-256 repositories, these values are computed using SHA-256") and git-index-pack.txt
# (`--index-version`).
set -euo pipefail
OUT=$(cd "$1" && pwd)
unset GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 TZ=UTC LC_ALL=C LANG=C
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_AUTHOR_DATE='1112911993 +0000' GIT_COMMITTER_DATE='1112911993 +0000'
WORK=$(mktemp -d "$HOME/.cache/slates-gitidx.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"; mkdir -p "$HOME"
git --version > "$WORK/git-version.txt"
cmp "$WORK/git-version.txt" "$OUT/git-version.txt"
# 1. Outside any repository, so no repository's configuration reaches index-pack; verify-pack finds a pack by its
# index's name, so the copies carry the names the listing prints.
mkdir "$WORK/large"; cd "$WORK/large"
cp "$OUT/ofs.pack" plain.pack
git index-pack --no-rev-index -o plain.idx plain.pack > /dev/null
cmp plain.idx "$OUT/ofs.idx"
cp "$OUT/ofs.pack" ofs-large.pack
git index-pack --index-version=2,0x200 --no-rev-index -o ofs-large.idx ofs-large.pack > /dev/null
git verify-pack -v ofs-large.idx > ofs-large.verify.txt
cp ofs-large.idx ofs-large.verify.txt "$OUT/"
# 2. A SHA-256 repository with the first repository's history: a long text edited twice (stored as deltas), a
# subdirectory, an annotated tag and a second branch.
cd "$WORK"
git init -q --object-format=sha256 -b main repo
cd repo
git config commit.gpgsign false; git config tag.gpgsign false
for i in $(seq 1 200); do echo "line $i of a text long enough to be stored as a delta"; done > text
git add text; git commit -q -m 'first'
sed -i.bak 's/^line 100 of/line one hundred of/' text && rm text.bak
mkdir dir; printf 'deep\n' > dir/file; git add text dir/file; git commit -q -m 'second'
sed -i.bak 's/^line 7 of/line seven of/' text && rm text.bak
git add text; git commit -q -m 'third'
git tag -a -m 'version one' v1 HEAD~1
git branch side HEAD~2
git rev-list --objects --all | cut -d' ' -f1 > "$WORK/objects"
git pack-objects -q --delta-base-offset --window=10 --depth=50 "$WORK/sha256" < "$WORK/objects" > "$WORK/sha256.name"
mkdir "$WORK/listing"; cd "$WORK/listing"
cp "$WORK/sha256-$(cat "$WORK/sha256.name").pack" sha256.pack
cp "$WORK/sha256-$(cat "$WORK/sha256.name").idx" sha256.idx
git verify-pack --object-format=sha256 -v sha256.idx > sha256.verify.txt
cp sha256.pack sha256.idx sha256.verify.txt "$OUT/"
