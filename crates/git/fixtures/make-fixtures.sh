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
# A name whose refusal is longer than the 128 bytes git's server relays a read of `pack-objects`' errors in, holding a
# tab, which `die` keeps, and a control byte and DEL, which it shows as `?`.
LONG="main:$(printf 'a%.0s' $(seq 1 140))"$'\t\x01\x7fz'
{ pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
  pkt $'thin-pack\n'; pkt $'ofs-delta\n'; pkt $'no-progress\n'; pkt "want $HEAD_OID"$'\n'
  pkt "filter sparse:oid=$LONG"$'\n'; pkt $'done\n'; printf 0000; } > "$OUT/fetch-sparse-long.request.bin"
v2 < "$OUT/fetch-sparse-long.request.bin" > "$OUT/fetch-sparse-long.response.bin" 2> /dev/null || true
# The refusal under `sideband-all`, alone and with `packfile-uris`: git writes the packfile section's header before
# `pack-objects` begins when the request names no URI protocol, and otherwise only once its pack begins, which a refusal
# never does.
git config uploadpack.allowSidebandAll true
{ pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
  pkt $'ofs-delta\n'; pkt $'no-progress\n'; pkt $'sideband-all\n'; pkt "want $HEAD_OID"$'\n'
  pkt $'filter sparse:oid=main:missing\n'; pkt $'done\n'; printf 0000; } > "$OUT/fetch-sparse-sideband-all.request.bin"
v2 < "$OUT/fetch-sparse-sideband-all.request.bin" > "$OUT/fetch-sparse-sideband-all.response.bin" 2> /dev/null || true
git config uploadpack.blobPackfileUri "$BLOB $URIPACK https://example.invalid/$URIPACK.pack"
{ pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
  pkt $'ofs-delta\n'; pkt $'no-progress\n'; pkt $'sideband-all\n'; pkt "want $HEAD_OID"$'\n'; pkt $'packfile-uris https\n'
  pkt $'filter sparse:oid=main:missing\n'; pkt $'done\n'; printf 0000; } > "$OUT/fetch-sparse-uris.request.bin"
v2 < "$OUT/fetch-sparse-uris.request.bin" > "$OUT/fetch-sparse-uris.response.bin" 2> /dev/null || true
git config --unset uploadpack.blobPackfileUri; git config --unset uploadpack.allowSidebandAll
git config --unset uploadpack.allowFilter
# What git writes to its error stream while it reads a `sparse:oid` filter's name (A-640 part 4, fourth slice), relayed
# on band 2 before the pack or before the lines it dies with. Each capture's extra refs are made for it alone and
# deleted after it, so every capture above and below is served from the same refs. `sparse NAME BLOB-ISH` writes a
# clone's request under `sparse:oid=BLOB-ISH` to `fetch-sparse-NAME.request.bin` and git's answer beside it.
git config uploadpack.allowFilter true
sparse() {
  { pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
    pkt $'thin-pack\n'; pkt $'ofs-delta\n'; pkt $'no-progress\n'; pkt "want $HEAD_OID"$'\n'
    pkt "filter sparse:oid=$2"$'\n'; pkt $'done\n'; printf 0000; } > "$OUT/fetch-sparse-$1.request.bin"
  v2 < "$OUT/fetch-sparse-$1.request.bin" > "$OUT/fetch-sparse-$1.response.bin" 2> /dev/null || true
}
# Two of git's patterns find a ref named `main`, the tag first: git warns, and walks the tag's commit, main's head.
git update-ref refs/tags/main "$HEAD_OID"
sparse warn-two-refs main:dir/file
git update-ref -d refs/tags/main
# A branch named `4cdb`, which also abbreviates one object's name, the blob `dir/file`'s: git warns, and takes the
# branch's commit, which is no blob.
git update-ref refs/heads/4cdb "$HEAD_OID"
sparse warn-ref-and-object 4cdb
git update-ref -d refs/heads/4cdb
# A branch named by main's head's own full name: git warns, prints its advice, and takes the object that name names.
git update-ref "refs/heads/$HEAD_OID" "$HEAD_OID"
sparse warn-full-name "$HEAD_OID:dir/file"
git update-ref -d "refs/heads/$HEAD_OID"
# Abbreviated names: the blob's in four digits and in eight upper case; the root tree's, which is no blob; main's head's
# before a path; the blob's whole name with a `0` after it, which git reads as its name's padding, and with a `1`; and
# `git describe`'s name for the blob, whose ref part names no ref.
sparse abbrev-blob 4cdb
sparse abbrev-upper 4CDB2265
sparse abbrev-tree 0351
sparse abbrev-rev d0f1:dir/file
sparse abbrev-padded 4cdb2265d30204be5463b38174b2e8e7179824050
sparse abbrev-unpadded 4cdb2265d30204be5463b38174b2e8e7179824051
sparse describe v1-1-g4cdb
# Objects whose names begin alike (their contents found by a search of names): two blobs whose names share eight
# digits, `5ad7d2ca`, in a tree under a commit of their own; two commits sharing four, `67fd`, each by an author five
# and a half hours east of UTC with a subject of two lines; two tags sharing four, `2188`. Each is reached by a ref;
# the tests make the eight from these same bytes (`upload/tests/names.rs`), so their names are checked here.
BLOB_A=$(printf 'collide 26350\n' | git hash-object -w --stdin)
BLOB_B=$(printf 'collide 62386\n' | git hash-object -w --stdin)
PAIR=$(printf '100644 blob %s\ta\n100644 blob %s\tb\n' "$BLOB_A" "$BLOB_B" | git mktree)
ROOT=$(git rev-parse "$HEAD_OID^{tree}")
IDENT='test <test@example.com> 1112911993'
COLLISIONS=$(printf 'tree %s\nparent %s\nauthor %s +0000\ncommitter %s +0000\n\ncollisions\n' \
  "$PAIR" "$HEAD_OID" "$IDENT" "$IDENT" | git hash-object -t commit -w --stdin)
git update-ref refs/heads/collide-blobs "$COLLISIONS"
ALIKE="$BLOB_A $BLOB_B $PAIR $COLLISIONS"
for n in 136 151; do
  made=$(printf 'tree %s\nparent %s\nauthor %s +0530\ncommitter %s +0000\n\ncollide %d\nsecond line  \n\nbody\n' \
    "$ROOT" "$HEAD_OID" "$IDENT" "$IDENT" "$n" | git hash-object -t commit -w --stdin)
  git update-ref "refs/heads/collide-$n" "$made"; ALIKE="$ALIKE $made"
done
for n in 48 106; do
  made=$(printf 'object %s\ntype commit\ntag collide-%d\ntagger %s +0530\n\ncollide %d\n' \
    "$HEAD_OID" "$n" "$IDENT" "$n" | git hash-object -t tag -w --stdin)
  git update-ref "refs/tags/collide-$n" "$made"; ALIKE="$ALIKE $made"
done
BEGINS=$(printf '%s\n' $ALIKE | cut -c1-8 | tr '\n' ' ')
[ "$BEGINS" = '5ad7d2ca 5ad7d2ca 3af7e09f c6d52c1b 67fd0f47 67fdbfcb 2188e00e 21887657 ' ] \
  || { echo "the alike names changed: $ALIKE" >&2; exit 1; }
# Ambiguous abbreviations: the blobs' (git lists the two blobs); the commits' (no blob among them, so git lists every
# one), and the same before a path (both commits are trees' commits, so git lists both); and the tags'.
sparse ambiguous-blobs 5ad7
sparse ambiguous-commits 67fd
sparse ambiguous-commits-path 67fd:dir/file
sparse ambiguous-tags 2188
git config --unset uploadpack.allowFilter
# Revision suffixes (A-640 part 4, fifth slice): parents and ancestors, peels, and `@{...}` with git's branch marks;
# `67fd~1` reads the alike commits' abbreviation for a commit. A served repository is bare, and a bare repository logs
# no ref update (`core.logAllRefUpdates`), so the logs this working repository kept are removed for these captures.
git config core.logAllRefUpdates false
rm -rf .git/logs
git config uploadpack.allowFilter true
while read -r name blob_ish; do sparse "$name" "$blob_ish"; done <<'NAMES'
ancestor main~1:dir/file
ancestor-bare main~:dir/file
parent main^:dir/file
parent-zero main^0:dir/file
parent-second main^2:dir/file
ancestor-past main~3
ancestor-pathless main~2:dir/file
ancestor-huge main~2147483648
ancestor-head @~1:dir/file
parent-tag v1^0:dir/file
ancestor-blob 4cdb2265d30204be5463b38174b2e8e717982405~1
ancestor-ambiguous 67fd~1:dir/file
peel-tree main^{tree}:dir/file
peel-commit main^{commit}:dir/file
peel-deref v1^{}:dir/file
peel-tag v1^{tag}
peel-blob-mismatch main^{blob}
peel-blob 4cdb^{blob}
peel-object main^{object}
peel-unknown main^{foo}
peel-trailing main^{commit}x}
peel-search-empty main^{/}:dir/file
reflog-branch main@{1}:dir/file
reflog-head-zero @{0}:dir/file
reflog-head-one @{1}
reflog-head-date @{yesterday}
reflog-head-word @{garbage}
reflog-head-month @{jan}
reflog-named-head HEAD@{0}:dir/file
prior @{-1}:dir/file
prior-late main@{-1}
upstream main@{upstream}
upstream-short @{u}
upstream-nosuch nosuch@{u}
upstream-path main@{upstream}:dir/file
push main@{push}
push-nosuch nosuch@{push}
NAMES
git config --unset uploadpack.allowFilter
