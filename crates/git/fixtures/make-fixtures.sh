#!/usr/bin/env bash
# Makes the golden vectors of slates-git (A-582) with stock git, captured on its own wire: protocol v2's capability
# advertisement, `ls-refs` and `fetch` requests (written here byte for byte) with stock `git upload-pack`'s answers to
# each, and packs `git pack-objects` wrote with offset and with reference deltas, their `.idx` from `git index-pack` and
# `git verify-pack -v`'s listing of each. Deterministic: no configuration but this script's (system, global and the
# environment's `GIT_CONFIG_*` all cleared), a fixed identity, every date 2005-04-07T22:13:13Z.
# Usage: make-fixtures.sh OUTDIR
# Every file is the same on every run but `fetch-progress.response.bin`, whose progress text git throttles by time;
# its framing is what the tests read.
#
# Sources: git's Documentation/gitprotocol-common.txt (pkt-line), gitprotocol-v2.txt (capabilities, ls-refs, fetch and
# its sections, side-band-64k in the packfile section), gitformat-pack.txt (packs and idx v2).
set -euo pipefail
OUT=$(cd "$1" && pwd)
unset GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 TZ=UTC LC_ALL=C LANG=C
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_AUTHOR_DATE='1112911993 +0000' GIT_COMMITTER_DATE='1112911993 +0000'
WORK=$(mktemp -d "$HOME/.cache/g251b-gitfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"; mkdir -p "$HOME"
git --version > "$OUT/git-version.txt"
cd "$WORK"
git init -q -b main repo
cd repo
git config commit.gpgsign false; git config tag.gpgsign false
git config uploadpack.allowRefInWant true
# A text long enough that its edits are stored as deltas against it.
for i in $(seq 1 200); do echo "line $i of a text long enough to be stored as a delta"; done > text
git add text; git commit -q -m 'first'
sed -i.bak 's/^line 100 of/line one hundred of/' text && rm text.bak
mkdir dir; printf 'deep\n' > dir/file; git add text dir/file; git commit -q -m 'second'
sed -i.bak 's/^line 7 of/line seven of/' text && rm text.bak
git add text; git commit -q -m 'third'
git tag -a -m 'version one' v1 HEAD~1
git branch side HEAD~2
git log --format='%H %P %s' --all > "$OUT/log.txt"
git for-each-ref --format='%(objectname) %(objecttype) %(refname)' > "$OUT/refs.txt"
HEAD_OID=$(git rev-parse main); FIRST=$(git rev-parse main~2); SECOND=$(git rev-parse main~1)
pkt() { printf '%04x%s' $(( ${#1} + 4 )) "$1"; }
v2() { GIT_PROTOCOL=version=2 git upload-pack --stateless-rpc "$@" .; }
# git 2.41 renamed `--advertise-refs` to `--http-backend-info-refs`; either answers what `info/refs` does.
adv() { GIT_PROTOCOL=version=2 git upload-pack --http-backend-info-refs --stateless-rpc . 2>/dev/null \
  || GIT_PROTOCOL=version=2 git upload-pack --advertise-refs --stateless-rpc .; }
# The capability advertisement, as `git-http-backend` answers `info/refs?service=git-upload-pack` for version 2.
adv > "$OUT/advertisement.bin"
{ pkt $'command=ls-refs\n'; pkt $'object-format=sha1\n'; printf 0001
  pkt $'peel\n'; pkt $'symrefs\n'; pkt $'unborn\n'; pkt $'ref-prefix HEAD\n'; pkt $'ref-prefix refs/\n'
  printf 0000; } > "$OUT/ls-refs.request.bin"
v2 < "$OUT/ls-refs.request.bin" > "$OUT/ls-refs.response.bin"
{ pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
  pkt $'thin-pack\n'; pkt $'ofs-delta\n'; pkt $'no-progress\n'; pkt "want $HEAD_OID"$'\n'; pkt $'done\n'
  printf 0000; } > "$OUT/fetch-clone.request.bin"
v2 < "$OUT/fetch-clone.request.bin" > "$OUT/fetch-clone.response.bin"
{ pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
  pkt $'thin-pack\n'; pkt $'ofs-delta\n'; pkt $'no-progress\n'; pkt "want $HEAD_OID"$'\n'; pkt "have $FIRST"$'\n'
  printf 0000; } > "$OUT/fetch-haves.request.bin"
v2 < "$OUT/fetch-haves.request.bin" > "$OUT/fetch-haves.response.bin"
{ pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
  pkt $'ofs-delta\n'; pkt $'no-progress\n'; pkt "want $HEAD_OID"$'\n'; pkt $'deepen 1\n'; pkt $'done\n'
  printf 0000; } > "$OUT/fetch-shallow.request.bin"
v2 < "$OUT/fetch-shallow.request.bin" > "$OUT/fetch-shallow.response.bin"
{ pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
  pkt $'ofs-delta\n'; pkt $'no-progress\n'; pkt $'want-ref refs/heads/side\n'; pkt $'done\n'
  printf 0000; } > "$OUT/fetch-ref-in-want.request.bin"
v2 < "$OUT/fetch-ref-in-want.request.bin" > "$OUT/fetch-ref-in-want.response.bin"
{ pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
  pkt $'ofs-delta\n'; pkt "want $HEAD_OID"$'\n'; pkt "have $SECOND"$'\n'; pkt $'done\n'
  printf 0000; } > "$OUT/fetch-progress.request.bin"
v2 < "$OUT/fetch-progress.request.bin" > "$OUT/fetch-progress.response.bin"
# packfile-uris: the blob `dir/file` served from a pack of its own, named by the pack's hash. Stock git sends URIs only
# in a response multiplexed throughout (`sideband-all`; git's own t5702 sets GIT_TEST_SIDEBAND_ALL for it), so the
# server allows it and the request asks for it.
BLOB=$(git rev-parse main:dir/file)
URIPACK=$(echo "$BLOB" | git pack-objects -q "$WORK/uri")
git config uploadpack.blobPackfileUri "$BLOB $URIPACK https://example.invalid/$URIPACK.pack"
git config uploadpack.allowSidebandAll true
adv > "$OUT/advertisement-uris.bin"
{ pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
  pkt $'ofs-delta\n'; pkt $'no-progress\n'; pkt $'sideband-all\n'; pkt "want $HEAD_OID"$'\n'; pkt $'packfile-uris https\n'
  pkt $'done\n'
  printf 0000; } > "$OUT/fetch-uris.request.bin"
v2 < "$OUT/fetch-uris.request.bin" > "$OUT/fetch-uris.response.bin"
git config --unset uploadpack.blobPackfileUri; git config --unset uploadpack.allowSidebandAll
# Packs with offset deltas and with reference deltas, their indexes and git's own listing of each.
git rev-list --objects --all | cut -d' ' -f1 > "$WORK/objects"
git pack-objects -q --delta-base-offset --window=10 --depth=50 "$WORK/ofs" < "$WORK/objects" > "$WORK/ofs.name"
git pack-objects -q --window=10 --depth=50 "$WORK/ref" < "$WORK/objects" > "$WORK/ref.name"
cp "$WORK/ofs-$(cat "$WORK/ofs.name").pack" "$OUT/ofs.pack"; cp "$WORK/ofs-$(cat "$WORK/ofs.name").idx" "$OUT/ofs.idx"
cp "$WORK/ref-$(cat "$WORK/ref.name").pack" "$OUT/ref.pack"; cp "$WORK/ref-$(cat "$WORK/ref.name").idx" "$OUT/ref.idx"
# From inside OUT, so the listing names the pack by its own name, not by this run's directory.
(cd "$OUT" && git verify-pack -v ofs.idx > ofs.verify.txt && git verify-pack -v ref.idx > ref.verify.txt)
for object in $(cat "$WORK/objects"); do printf '%s %s %s\n' "$object" "$(git cat-file -t "$object")" "$(git cat-file -s "$object")"; done > "$OUT/objects.txt"
# A sparse filter (A-640 part 4; `git clone --filter=sparse:oid=...`), with filters allowed for these alone:
# `main:dir/file` names a blob whose one pattern, `deep`, matches no path, so the pack holds every commit and tree and
# no blob; `main:missing` names nothing and `main:dir` a tree, for git's two refusals ("unable to access sparse blob",
# "unable to parse sparse filter data").
git config uploadpack.allowFilter true
for name in main:dir/file main:missing main:dir; do
  file="$OUT/fetch-sparse-$(printf '%s' "$name" | tr ':/' '--')"
  { pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
    pkt $'thin-pack\n'; pkt $'ofs-delta\n'; pkt $'no-progress\n'; pkt "want $HEAD_OID"$'\n'
    pkt "filter sparse:oid=$name"$'\n'; pkt $'done\n'; printf 0000; } > "$file.request.bin"
  v2 < "$file.request.bin" > "$file.response.bin" 2> /dev/null || true
done
git config --unset uploadpack.allowFilter
