#!/usr/bin/env bash
# Makes slates-git's receive-pack golden vectors (A-620): stock git pushes to stock `git receive-pack`, the bytes each
# side sent captured by a wrapper between them (`git push --receive-pack=<wrapper>`, which git runs with the
# repository's path as a local transport runs `git-receive-pack`):
# - <scenario>.request.bin: what `git push` sent, its commands (with its capabilities and any push options) and its pack;
# - <scenario>.response.bin: what `git receive-pack` answered, its ref advertisement and its report;
# - <scenario>.before.refs and <scenario>.before.pack: the server's refs before the push, and every object it held as
#   one pack, from which a test makes the repository the push met.
# The scenarios, in order on one bare server that refuses non-fast-forwards and advertises push options:
# - create: `main` pushed to the empty repository;
# - update: a fast-forward of `main` that changes one line of a long text, which git sends as a delta against the
#   server's own copy of the text, a base the pack does not hold (a thin pack);
# - tag: an annotated tag pushed, naming the new commit;
# - copy: a branch created at a commit the server holds, with the empty pack the protocol asks for;
# - side: a branch created at a new commit;
# - delete: `side` deleted, with no pack;
# - options: a fast-forward of `main` with two push options;
# - non-ff: a rewritten `main` forced, which the server refuses as non-fast-forward;
# - atomic: `--atomic` of a new branch and the rewritten `main` forced, both refused;
# - hidden: with `receive.hideRefs refs/hidden` and a ref under it, a push to another ref under it, refused as a hidden
#   ref's update, the hidden ref left out of the advertisement;
# - fsck: with `receive.fsckObjects` too, a commit with no author email (`git hash-object --literally`), refused by
#   index-pack's strict check.
# Deterministic: no configuration but this script's, a fixed identity, fixed dates, one thread of delta search; then
# SHA256SUMS lists every file made.
# Usage: make-push-fixtures.sh OUTDIR
#
# Sources: git's Documentation/gitprotocol-pack.adoc ("Pushing Data To a Server", "Report Status"),
# builtin/receive-pack.c and send-pack.c, v2.50.1.
set -euo pipefail
OUT=$(cd "$1" && pwd)
unset GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE GIT_PROTOCOL
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 TZ=UTC LC_ALL=C LANG=C
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
WORK=$(mktemp -d "$HOME/.cache/slates-git-pushfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"; mkdir -p "$HOME"
git --version > "$OUT/git-version.txt"
SERVER="$WORK/server.git"
git init -q --bare -b main "$SERVER"
git -C "$SERVER" config receive.denyNonFastForwards true
git -C "$SERVER" config receive.advertisePushOptions true
WRAP="$WORK/wrap.sh"
cat > "$WRAP" <<'SH'
#!/bin/sh
tee "$CAPTURE.request.bin" | git receive-pack "$@" | tee "$CAPTURE.response.bin"
SH
chmod +x "$WRAP"
git init -q -b main "$WORK/client"
cd "$WORK/client"
git config commit.gpgsign false; git config tag.gpgsign false
at=1112911993
commit() { at=$((at + 60)); GIT_AUTHOR_DATE="$at +0000" GIT_COMMITTER_DATE="$at +0000" git commit -q "$@"; }
# The server's refs and objects before a push, then the push through the wrapper; a refused push is a capture too.
push() {
  scenario=$1; shift
  git -C "$SERVER" for-each-ref --format='%(objectname) %(refname)' > "$OUT/$scenario.before.refs"
  git -C "$SERVER" rev-list --objects --all | cut -d' ' -f1 \
    | git -C "$SERVER" -c pack.threads=1 pack-objects -q --stdout > "$OUT/$scenario.before.pack"
  CAPTURE="$OUT/$scenario" git -c pack.threads=1 push -q --receive-pack="$WRAP" "$@" || true
}
for i in $(seq 1 200); do echo "line $i of a text long enough to be stored as a delta"; done > text
git add text; commit -m 'first'
push create "$SERVER" main
sed -i.bak 's/^line 100 of/line one hundred of/' text && rm text.bak
git add text; commit -m 'second'
push update "$SERVER" main
GIT_COMMITTER_DATE="$((at + 30)) +0000" git tag -a -m 'version one' v1
push tag "$SERVER" v1
push copy "$SERVER" main:refs/heads/copy
git checkout -q -b side
printf 'side\n' > side; git add side; commit -m 'side'
push side "$SERVER" side
push delete "$SERVER" :refs/heads/side
git checkout -q main
sed -i.bak 's/^line 7 of/line seven of/' text && rm text.bak
git add text; commit -m 'third'
push options -o ci.skip -o 'note=two words' "$SERVER" main
printf 'rewritten\n' > other; git add other; commit --amend -m 'third, rewritten'
push non-ff --force "$SERVER" main
git checkout -q -b good main~1
printf 'good\n' > good; git add good; commit -m 'good'
git checkout -q main
push atomic --atomic --force "$SERVER" good main
git -C "$SERVER" config receive.hideRefs refs/hidden
git -C "$SERVER" update-ref refs/hidden/x "$(git -C "$SERVER" rev-parse main)"
push hidden "$SERVER" main:refs/hidden/y
git -C "$SERVER" config receive.fsckObjects true
bad=$(printf 'tree %s\nauthor A %s +0000\ncommitter A <a@example.com> %s +0000\n\nno author email\n' \
  "$(git rev-parse 'main^{tree}')" "$at" "$at" | git hash-object -t commit -w --literally --stdin)
git update-ref refs/heads/bad "$bad"
push fsck "$SERVER" bad
cd "$OUT" && shasum -a 256 git-version.txt ./*.bin ./*.refs ./*.pack | sed 's| \./| |' > SHA256SUMS
