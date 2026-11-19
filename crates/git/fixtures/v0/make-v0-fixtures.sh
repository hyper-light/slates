#!/usr/bin/env bash
# Makes slates-git's protocol v0 and v1 golden vectors (A-604) with stock git, captured on its own wire:
# - the ref advertisement `git upload-pack` writes under each version (v1 opening with `version 1`), of a repository with
#   branches and an annotated tag (its peeled line), of an empty one (the `capabilities^{}` form), of a shallow one (its
#   `shallow` lines), of a SHA-256 one, and `git receive-pack`'s;
# - whole exchanges between stock `git fetch-pack` and `git upload-pack` under protocol.version 0 and 1, each side's
#   bytes copied by a tee in the upload-pack command: a clone, a fetch with haves, a clone to depth 1, a shallow clone
#   deepened, clones by `--shallow-since` and `--shallow-exclude`, and a partial clone by `--filter=blob:none`;
# - requests written here byte for byte (stateless, as an HTTP server answers them) with upload-pack's answers: the
#   negotiation under multi_ack, under multi_ack_detailed ending `done` and reaching `ready`, and with neither; a client
#   shallow at the tip deepened to two (`unshallow`); a want never advertised (`ERR`); and a `sparse:oid` filter's
#   clone, and its refusal of a blob that names nothing, on side-band-64k and on no band (A-640 part 4).
# Then SHA256SUMS: every file's SHA-256, which the tests check before they read a byte.
# Deterministic: no configuration but this script's (system, global and the environment's `GIT_CONFIG_*` cleared), a
# fixed identity, every date 2005-04-07T22:13:13Z, packs made on one thread, and no progress.
# Usage: make-v0-fixtures.sh OUTDIR
#
# Sources: git's Documentation/gitprotocol-pack.adoc ("Reference Discovery", "Packfile Negotiation"),
# gitprotocol-capabilities.adoc; upload-pack.c, fetch-pack.c, connect.c, shallow.c.
set -euo pipefail
OUT=$(cd "$1" && pwd)
unset GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE GIT_PROTOCOL
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 TZ=UTC LC_ALL=C LANG=C
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_AUTHOR_DATE='1112911993 +0000' GIT_COMMITTER_DATE='1112911993 +0000'
WORK=$(mktemp -d "$HOME/.cache/slates-git-v0fix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"; mkdir -p "$HOME"
git --version > "$OUT/git-version.txt"
pkt() { printf '%04x%s' $(( ${#1} + 4 )) "$1"; }
# The upload-pack command the client runs: it copies what each side writes. git runs it through the shell with the
# repository's path, so `$1` is the path; CAPTURE names the copies. A command that runs upload-pack twice (a fetch
# that follows tags in a second round) fails here rather than keep only its last exchange.
cat > "$WORK/capture" <<'SH'
#!/bin/sh
if [ -e "$CAPTURE.request.bin" ]; then echo "a second upload-pack for $CAPTURE" >&2; exit 1; fi
tee "$CAPTURE.request.bin" | git upload-pack "$1" | tee "$CAPTURE.response.bin"
SH
chmod +x "$WORK/capture"
made() { git init -q -b main "$@"; }
setup() { git config commit.gpgsign false; git config tag.gpgsign false; git config pack.threads 1
  git config uploadpack.allowFilter true; }
# The server: three commits on main, one on a side branch, an annotated tag.
made "$WORK/server"; cd "$WORK/server"; setup
echo one > a; git add a; git commit -q -m one
echo two > b; git add b; git commit -q -m two
git branch side; echo three > c; git add c; git commit -q -m three
git tag -a -m 'version one' v1 HEAD~1
git checkout -q side; echo side > s; git add s; git commit -q -m side; git checkout -q main
adv() { git upload-pack --http-backend-info-refs --stateless-rpc .; }
adv > "$OUT/advertisement-v0.bin"
GIT_PROTOCOL=version=1 adv > "$OUT/advertisement-v1.bin"
git receive-pack --http-backend-info-refs . > "$OUT/advertisement-receive-pack.bin"
git log --format='%H %s' --all > "$OUT/log.txt"
git for-each-ref --format='%(objectname) %(refname)' > "$OUT/refs.txt"
TIP=$(git rev-parse main); TWO=$(git rev-parse main~1); ONE=$(git rev-parse main~2)
SIDE=$(git rev-parse side); UNKNOWN=$(printf 'not in the server\n' | git hash-object --stdin)
# An empty repository: the `capabilities^{}` form.
made "$WORK/empty"; (cd "$WORK/empty" && adv > "$OUT/advertisement-empty.bin")
# A SHA-256 repository.
git init -q -b main --object-format=sha256 "$WORK/sha256"
(cd "$WORK/sha256" && setup && echo one > a && git add a && git commit -q -m one && adv > "$OUT/advertisement-sha256.bin")
# Whole exchanges, each version.
cd "$WORK"
for v in 0 1; do
  CAPTURE="$OUT/clone-v$v" git -c protocol.version=$v clone -q --no-local --upload-pack="$WORK/capture" \
    "file://$WORK/server" "clone-v$v"
done
# A fetch with haves: a clone of the server's first two commits, the server one commit and a branch ahead.
git -c protocol.version=0 clone -q --no-local --single-branch -b main "file://$WORK/server" behind
(cd behind && git reset -q --hard HEAD~1 && git update-ref -d refs/remotes/origin/main && git gc -q --prune=now)
for v in 0 1; do
  cp -R behind "behind-v$v"
  (cd "behind-v$v" && CAPTURE="$OUT/fetch-v$v" git -c protocol.version=$v fetch -q \
    --upload-pack="$WORK/capture" origin)
done
CAPTURE="$OUT/depth-v0" git -c protocol.version=0 clone -q --no-local --depth 1 --upload-pack="$WORK/capture" \
  "file://$WORK/server" depth
# `--no-tags`: following the tag would take a second round, a second upload-pack.
(cd depth && CAPTURE="$OUT/deepen-v0" git -c protocol.version=0 fetch -q --no-tags --deepen=1 \
  --upload-pack="$WORK/capture" origin)
CAPTURE="$OUT/since-v0" git -c protocol.version=0 clone -q --no-local --shallow-since=2005-04-07T22:13:13Z \
  --upload-pack="$WORK/capture" "file://$WORK/server" since
CAPTURE="$OUT/exclude-v0" git -c protocol.version=0 clone -q --no-local --shallow-exclude=side \
  --upload-pack="$WORK/capture" "file://$WORK/server" exclude
# No checkout: the blobs it would read are the ones the filter left out, fetched lazily by another upload-pack.
CAPTURE="$OUT/filter-v0" git -c protocol.version=0 clone -q --no-local --no-checkout --filter=blob:none \
  --upload-pack="$WORK/capture" "file://$WORK/server" filter
# A shallow server's advertisement names its shallow commit.
(cd depth && adv > "$OUT/advertisement-shallow.bin")
# Requests written here, answered statelessly as an HTTP server answers them.
cd "$WORK/server"
up() { git upload-pack --stateless-rpc .; }
{ pkt "want $TIP multi_ack side-band-64k ofs-delta"$'\n'; printf 0000
  pkt "have $UNKNOWN"$'\n'; pkt "have $TWO"$'\n'; pkt "have $ONE"$'\n'; printf 0000; } > "$OUT/multi-ack.request.bin"
up < "$OUT/multi-ack.request.bin" > "$OUT/multi-ack.response.bin"
{ pkt "want $TIP side-band-64k ofs-delta"$'\n'; printf 0000
  pkt "have $UNKNOWN"$'\n'; pkt "have $TWO"$'\n'; pkt "have $ONE"$'\n'; printf 0000; } > "$OUT/plain.request.bin"
up < "$OUT/plain.request.bin" > "$OUT/plain.response.bin"
{ pkt "want $TIP multi_ack_detailed side-band-64k ofs-delta no-progress"$'\n'; pkt "want $SIDE"$'\n'; printf 0000
  pkt "have $UNKNOWN"$'\n'; pkt "have $TWO"$'\n'; pkt $'done\n'; } > "$OUT/detailed-done.request.bin"
up < "$OUT/detailed-done.request.bin" > "$OUT/detailed-done.response.bin"
# multi_ack_detailed's `ready`: a common have that leaves nothing to negotiate, then a flush.
{ pkt "want $TIP multi_ack_detailed side-band-64k ofs-delta"$'\n'; printf 0000; pkt "have $TWO"$'\n'; printf 0000
  } > "$OUT/ready.request.bin"
up < "$OUT/ready.request.bin" > "$OUT/ready.response.bin"
# shallow-update's `unshallow`: a client shallow at the tip asks for a depth of two (its shallow and deepen lines
# written as fetch-pack writes them, with no newline).
{ pkt "want $TIP multi_ack_detailed side-band-64k ofs-delta no-progress"$'\n'; pkt "shallow $TIP"; pkt 'deepen 2'
  printf 0000; pkt $'done\n'; } > "$OUT/unshallow.request.bin"
up < "$OUT/unshallow.request.bin" > "$OUT/unshallow.response.bin"
# A want the server never advertised: refused with `ERR`.
{ pkt "want $UNKNOWN side-band-64k ofs-delta"$'\n'; printf 0000; pkt $'done\n'; } > "$OUT/unknown-want.request.bin"
up < "$OUT/unknown-want.request.bin" > "$OUT/unknown-want.response.bin" 2> /dev/null || true
# A sparse filter (A-640 part 4): `main:a` names a blob whose one pattern, `one`, matches no path, so the pack holds the
# commits and trees alone; `main:missing` names nothing, which `pack-objects` dies of as it begins: its words on band 2
# and git's band-3 words after the `NAK` on side-band-64k, and with no band to say them on, the `NAK` alone.
{ pkt "want $TIP side-band-64k ofs-delta no-progress filter"$'\n'; pkt $'filter sparse:oid=main:a\n'; printf 0000
  pkt $'done\n'; } > "$OUT/sparse-v0.request.bin"
up < "$OUT/sparse-v0.request.bin" > "$OUT/sparse-v0.response.bin"
{ pkt "want $TIP side-band-64k ofs-delta no-progress filter"$'\n'; pkt $'filter sparse:oid=main:missing\n'
  printf 0000; pkt $'done\n'; } > "$OUT/sparse-missing-v0.request.bin"
up < "$OUT/sparse-missing-v0.request.bin" > "$OUT/sparse-missing-v0.response.bin" 2> /dev/null || true
{ pkt "want $TIP ofs-delta no-progress filter"$'\n'; pkt $'filter sparse:oid=main:missing\n'; printf 0000
  pkt $'done\n'; } > "$OUT/sparse-bare-v0.request.bin"
up < "$OUT/sparse-bare-v0.request.bin" > "$OUT/sparse-bare-v0.response.bin" 2> /dev/null || true
cd "$OUT"
find . -type f ! -name SHA256SUMS ! -name make-v0-fixtures.sh | LC_ALL=C sort | sed 's|^\./||' | xargs shasum -a 256 > SHA256SUMS
