#!/usr/bin/env bash
# Makes the golden vectors of A-665's read-only bare-repository view (`slates_git::store::hook_view`) with stock git:
# 1. `init.listing.txt`, `init.config`, `init.HEAD`: every path `git init --bare -b main` makes (`d` a directory, `f`
#    a file), and the bytes of the two files the view serves from them; `init-sha256.config` the same for
#    `--object-format=sha256`.
# 2. `bare.listing.txt`, `bare.packed-refs`, `bare.index-pack.txt`: A-582's repository (`../ofs.pack`, all twelve
#    objects, its refs as `../refs.txt` lists them) made a bare repository as git makes one: `git init --bare -b main`,
#    the objects put in by `git index-pack --stdin`, the refs by `git update-ref`, then `git pack-refs --all`. The index
#    `index-pack` wrote is compared with `../ofs.idx`, byte for byte, and the pack it kept with `../ofs.pack`.
# 3. `hook.<push>.<hook>.txt`: what stock receive-pack hands its hooks (`pre-receive`, `update`, `post-receive`) in
#    three pushes to that repository over git's local transport, each of one new commit on `main`: `loose` with git's
#    defaults (three objects, under `receive.unpackLimit`'s 100, so `unpack-objects` writes them loose into the
#    quarantine); `pack` with `receive.unpackLimit` 1 (`index-pack --stdin --keep` writes a pack there); `options` with
#    `receive.advertisePushOptions` and two `-o` options. Each file holds the hook's working directory, its arguments
#    and standard input, every `GIT_*` variable it sees, sorted, and for `pre-receive` every path under
#    `$GIT_QUARANTINE_PATH`. `pusher.env.txt` lists the `GIT_*` variables of the pushing process itself (this script's
#    own exports), which every hook inherits. The repository's absolute path is written `$ROOT`, git's own
#    `GIT_EXEC_PATH` (where this git is installed) `$EXEC_PATH`, and the six letters `mkdtemp` chose for the quarantine
#    `XXXXXX`; a `.keep` file's contents (a process id and a host name) are not kept.
# Deterministic, as `../make-fixtures.sh`: no configuration but this script's (system, global and the environment's
# `GIT_CONFIG_*` cleared), a fixed identity, every date 2005-04-07T22:13:13Z. Usage: make-view-fixtures.sh OUTDIR, run
# from anywhere; OUTDIR's parent holds `make-fixtures.sh`'s `ofs.pack`, `ofs.idx`, `refs.txt` and `git-version.txt`.
#
# Sources (git v2.50.1): setup.c (`create_default_files`, `initialize_repository_version`), tmp-objdir.c
# (`tmp_objdir_create`, `env_append`, `env_replace`), builtin/receive-pack.c (`run_and_feed_hook`, `run_update_hook`,
# `unpack`, `execute_commands`), builtin/index-pack.c (`final`, the pack named by its trailer, `.rev` written by
# default), refs/packed-backend.c (`PACKED_REFS_HEADER`, `write_packed_entry`), Documentation/githooks.adoc.
set -euo pipefail
OUT=$(cd "$1" && pwd)
FIXTURES=$(cd "$OUT/.." && pwd)
# Every `GIT_*` variable this script was started with goes, so a hook's listing holds receive-pack's and this script's
# alone, and nothing of the machine's own configuration (a caller's `GIT_CONFIG_KEY_*`, say) reaches a fixture.
for variable in $(env | sed -n 's/^\(GIT_[A-Za-z0-9_]*\)=.*/\1/p'); do unset "$variable"; done
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 TZ=UTC LC_ALL=C LANG=C
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_AUTHOR_DATE='1112911993 +0000' GIT_COMMITTER_DATE='1112911993 +0000'
WORK=$(mktemp -d "$HOME/.cache/slates-viewfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home" TMPDIR="$WORK/tmp"; mkdir -p "$HOME" "$TMPDIR"
git --version > "$WORK/git-version.txt"
cmp "$WORK/git-version.txt" "$FIXTURES/git-version.txt"

# Every path under the directory $1, sorted, each `d` or `f` and its path from there (BSD and GNU find alike).
listing() {
  (cd "$1" && find . -mindepth 1 | LC_ALL=C sort | while read -r path; do
    if [ -d "$path" ]; then echo "d ${path#./}"; else echo "f ${path#./}"; fi
  done)
}

# 1. What `git init --bare` makes.
cd "$WORK"
git init -q --bare -b main init.git
listing init.git > "$OUT/init.listing.txt"
cp init.git/config "$OUT/init.config"
cp init.git/HEAD "$OUT/init.HEAD"
git init -q --bare -b main --object-format=sha256 init-sha256.git
cp init-sha256.git/config "$OUT/init-sha256.config"

# 2. A-582's repository as a bare repository.
git init -q --bare -b main view.git
ROOT=$(cd view.git && pwd -P)
git -C view.git index-pack --stdin < "$FIXTURES/ofs.pack" > "$OUT/bare.index-pack.txt"
NAME=$(cut -f2 "$OUT/bare.index-pack.txt")
cmp "view.git/objects/pack/pack-$NAME.idx" "$FIXTURES/ofs.idx"
cmp "view.git/objects/pack/pack-$NAME.pack" "$FIXTURES/ofs.pack"
while read -r object _ ref; do git -C view.git update-ref "$ref" "$object"; done < "$FIXTURES/refs.txt"
git -C view.git pack-refs --all
listing view.git > "$OUT/bare.listing.txt"
cp view.git/packed-refs "$OUT/bare.packed-refs"

# 3. What receive-pack hands its hooks.
export VIEWFIX_HOOKS="$WORK/hooks"; mkdir -p "$VIEWFIX_HOOKS"
for hook in pre-receive update post-receive; do
  cat > "view.git/hooks/$hook" <<'HOOK'
#!/bin/sh
hook=$(basename "$0")
out="$VIEWFIX_HOOKS/$(cat "$VIEWFIX_HOOKS/push").$hook.txt"
{
  printf 'cwd %s\n' "$(pwd -P)"
  for argument in "$@"; do printf 'arg %s\n' "$argument"; done
  if [ "$hook" != update ]; then sed 's/^/stdin /'; fi
  env | grep '^GIT_' | LC_ALL=C sort | sed 's/^/env /'
  if [ -n "${GIT_QUARANTINE_PATH:-}" ]; then
    (cd "$GIT_QUARANTINE_PATH" && find . -mindepth 1 | LC_ALL=C sort | sed 's/^\.\//quarantine /')
  fi
} > "$out"
HOOK
  chmod +x "view.git/hooks/$hook"
done
env | grep '^GIT_' | LC_ALL=C sort > "$OUT/pusher.env.txt"
git clone -q view.git client
cd client
git config commit.gpgsign false
push() {
  echo "$1" > "$VIEWFIX_HOOKS/push"
  echo "$1" > "$1"; git add "$1"; git commit -q -m "$1"
  shift; git push -q "$@" origin main
}
push loose
git -C "$ROOT" config receive.unpackLimit 1
push pack
git -C "$ROOT" config receive.advertisePushOptions true
push options -o first -o 'second option'
cd "$WORK"
EXEC_PATH=$(git --exec-path)
for file in "$VIEWFIX_HOOKS"/*.txt; do
  sed -e "s|$ROOT|\$ROOT|g" -e "s|=$EXEC_PATH\$|=\$EXEC_PATH|" \
    -e 's/tmp_objdir-incoming-[A-Za-z0-9]\{6\}/tmp_objdir-incoming-XXXXXX/g' "$file" > "$OUT/hook.$(basename "$file")"
done
git -C client log --format='%H %P %s' main > "$OUT/pushed.log.txt"
