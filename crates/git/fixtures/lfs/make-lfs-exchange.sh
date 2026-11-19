#!/usr/bin/env bash
# Records the stock git-lfs client's exchange with `responder.py` (A-610): slates-git's LFS codecs are held to what the
# client sends and accepts. The client runs over a fresh repository: a push (lock verification, a batch upload, two
# PUTs, two verify callbacks), locks taken (two, then a conflict), listed (whole, by path, for verification; each list
# of two paged by a cursor), released (by path, then by id with --force), a pointer whose object was never uploaded,
# and a clone (batch downloads, two GETs, and a per-object 404). Every request and answer goes to `exchange.jsonl` in
# OUTDIR, one JSON object a line, with the git and git-lfs versions and the client's words for the lock conflict beside
# it. Deterministic but for two things: the responder's port inside the URLs it hands out, and the order of the
# concurrent PUTs and verify callbacks of one push. No configuration but this script's (system and global configuration
# cleared, HOME a scratch folder), a fixed identity, every date 2005-04-07T22:13:13Z, fixed file contents. Usage:
# make-lfs-exchange.sh OUTDIR. Needs git, git-lfs (Homebrew's `git-lfs` on the Mac Studio) and python3; writes only
# under OUTDIR and a scratch folder it removes. Run 2026-11-19 04:27Z on the Mac Studio with git-lfs 3.8.1 (Homebrew,
# darwin arm64, go 1.27.3) and git 2.50.1: 24 exchanges; the clone's checkout failed on ghost.bin as intended, the
# client having read the per-object error ("[404] Object does not exist").
#
# Sources: git-lfs v3.8.1 (tag 36e29152, commit a662a6dbf, 2026-10-22): docs/api/batch.md, basic-transfers.md,
# locking.md; the client's request structs in tq/api.go and locking/api.go.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(cd "$1" && pwd)
WORK=$(mktemp -d "$HOME/.cache/slates-lfsx.XXXXXX")
RESPONDER=""
cleanup() { [ -n "$RESPONDER" ] && kill "$RESPONDER" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT
unset GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE
export GIT_CONFIG_NOSYSTEM=1 TZ=UTC LC_ALL=C LANG=C GIT_TERMINAL_PROMPT=0
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_AUTHOR_DATE='1112911993 +0000' GIT_COMMITTER_DATE='1112911993 +0000'
export HOME="$WORK/home"; mkdir -p "$HOME"
git --version > "$OUT/git-version.txt"
git lfs version > "$OUT/git-lfs-version.txt"
python3 -I "$HERE/responder.py" "$WORK/port" "$WORK/exchange.jsonl" &
RESPONDER=$!
for _ in $(seq 1 100); do [ -s "$WORK/port" ] && break; sleep 0.1; done
PORT=$(cat "$WORK/port")
git config --global init.defaultBranch main
git config --global lfs.url "http://127.0.0.1:$PORT/repo.git/info/lfs"
git config --global lfs.locksverify true
git lfs install --skip-repo > /dev/null
git init -q --bare "$WORK/remote.git"
git init -q "$WORK/work"
cd "$WORK/work"
git lfs install > /dev/null
git lfs track '*.bin' > /dev/null
printf 'large file one\n' > one.bin
printf 'large file two\n' > two.bin
git add .gitattributes one.bin two.bin
git commit -q -m first
git remote add origin "$WORK/remote.git"
git push -q origin main 2>&1 | sed 's/^/push: /'
git lfs lock one.bin | sed 's/^/lock: /'
git lfs lock two.bin | sed 's/^/lock: /'
if git lfs lock one.bin > "$OUT/conflict.txt" 2>&1; then echo "a second lock on one.bin was granted"; exit 1; fi
git lfs locks | sed 's/^/locks: /'
git lfs locks --path=one.bin | sed 's/^/locks by path: /'
git lfs locks --verify | sed 's/^/locks verify: /'
git lfs unlock one.bin | sed 's/^/unlock: /'
TWO=$(git lfs locks --json | python3 -I -c 'import json, sys; print(json.load(sys.stdin)[0]["id"])')
git lfs unlock --force --id="$TWO" | sed 's/^/unlock --force: /'
# A pointer whose object was never uploaded: its download is answered per object with 404.
git config lfs.allowincompletepush true
GHOST=$(printf 'never uploaded\n' | shasum -a 256 | cut -d' ' -f1)
printf 'version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize 15\n' "$GHOST" > ghost.bin
git add ghost.bin
git commit -q -m ghost
git push -q origin main 2>&1 | sed 's/^/push ghost: /'
cd "$WORK"
if git clone -q "$WORK/remote.git" clone > "$WORK/clone.txt" 2>&1; then echo "clone: smudged every file"; else echo "clone: exit $? (ghost.bin's object is missing, as intended)"; fi
grep -o '\[404\] Object does not exist' "$WORK/clone.txt" | head -1 | sed 's/^/clone: the client read /' || true
kill "$RESPONDER"; wait "$RESPONDER" 2>/dev/null || true; RESPONDER=""
cp "$WORK/exchange.jsonl" "$OUT/exchange.jsonl"
