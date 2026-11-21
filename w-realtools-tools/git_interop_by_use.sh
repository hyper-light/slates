#!/usr/bin/env bash
# w-realtools, goal 93 correctness by use (main 01a1ff2e, 2026-11-21): stock git against slates' git server with a
# version-control store, on the machine it runs on (jupiter's Git Bash, or a Mac). One fresh release
# `slates anchor --vcs-http 127.0.0.1:0 --vcs-store OUT/store`; each repository pushed whole to slates, then read back by
# stock git and compared with the source byte for byte: every ref, every object, `fsck --full --strict`, the checked-out
# tree (under autocrlf true and false, against a clone of the source made with the same settings), a pull of a new
# commit and tag pushed from a second clone, and a tag deleted by push.
#
# Usage: git_interop_by_use.sh SLATES OUT [GITGIT]
#   SLATES  the release `slates` binary;  OUT  a directory made here (deleted first);  GITGIT  a bare clone of
#   git/git (optional: without it only the Windows-paths repository runs)
# Every line that judges something starts with OK or RED; the run ends with a count of each.
set -uo pipefail
SLATES=$1 OUT=$2 GITGIT=${3:-}
STEP_S=${STEP_S:-1800}
rm -rf "$OUT" && mkdir -p "$OUT" && OUT=$(cd "$OUT" && pwd) || exit 1
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$OUT/gitconfig" GIT_TERMINAL_PROMPT=0 LC_ALL=C
git config --global user.name realtools && git config --global user.email realtools@example.invalid
git config --global init.defaultBranch main && git config --global core.longpaths true
git config --global protocol.file.allow always
oks=0 reds=0
ok() { oks=$((oks + 1)); echo "OK   $*"; }
red() { reds=$((reds + 1)); echo "RED  $*"; }
# A step under a bound: coreutils' timeout where there is one (Git Bash has it), perl's alarm elsewhere (macOS).
bounded() {
  if command -v timeout > /dev/null; then timeout "$STEP_S" "$@"; else perl -e 'alarm shift; exec @ARGV' "$STEP_S" "$@"; fi
}
now() { perl -MTime::HiRes=time -e 'printf "%.2f\n", time'; }
windows() { [ -n "${MSYSTEM:-}" ]; }

instance="w-realtools-git-$$" log="$OUT/anchor.log"
mkdir -p "$OUT/store"
"$SLATES" --instance "$instance" anchor --vcs-http 127.0.0.1:0 --vcs-store "$OUT/store" > /dev/null 2> "$log" &
ANCHOR=$!
stop_anchor() {
  if windows && [ -r "/proc/$ANCHOR/winpid" ]; then
    taskkill //F //T //PID "$(cat "/proc/$ANCHOR/winpid")" > /dev/null 2>&1
  fi
  kill -9 "$ANCHOR" 2> /dev/null
}
trap stop_anchor EXIT
for _ in $(seq 1 1200); do
  grep -q "^slates daemon: instance $instance serving on" "$log" && grep -q '^slates anchor: issuer surface: export ' "$log" && break
  sleep 0.1
done
first() { sed -n "$2" "$1" | head -1; }
port=$(first "$log" 's/^slates daemon: the version-control HTTP edge serves cleartext on 127\.0\.0\.1:\([0-9]*\).*/\1/p')
exports=$(first "$log" 's/^slates anchor: issuer surface: export //p' | tr -d '\r')
[ -n "$port" ] && [ -n "$exports" ] || { red "the anchor never served (log: $log)"; tail -20 "$log"; exit 1; }
streak=0
for _ in $(seq 1 1200); do
  if "$SLATES" --instance "$instance" volume list > /dev/null 2>&1; then streak=$((streak + 1)); else streak=0; fi
  [ "$streak" -ge 10 ] && break
  sleep 0.1
done
"$SLATES" --instance "$instance" bootstrap root > /dev/null || { red "bootstrap root"; exit 1; }
# shellcheck disable=SC2086 # the surface's NAME=VALUE pairs are words
token=$(env $exports "$SLATES" --instance "$instance" credential token | sed -n 's/^token: //p' | tr -d '\r')
[ -n "$token" ] || { red "no token minted"; exit 1; }
G=(git -c "http.extraHeader=Authorization: Bearer $token")
echo "slates: $("$SLATES" --version 2>&1 | head -1 | tr -d '\r'); git: $(git --version); port $port; $(uname -s)"

# The refs of a repository, one `<id> <name>` a line, sorted: what ls-remote says of a remote, show-ref of a local one.
refs_remote() { bounded "${G[@]}" ls-remote "$1" | grep -v ' HEAD$' | grep -v $'\tHEAD$' | tr '\t' ' ' | sort; }
# Branches and tags only: what the first push sends (a GitHub mirror also holds refs/pull/*, never pushed here).
refs_local() { git -C "$1" for-each-ref --format='%(objectname) %(refname)' refs/heads refs/tags | sort; }
objects() { git -C "$1" rev-list --branches --tags --objects | cut -d' ' -f1 | sort -u; }

# Compares the slates repository NAME (at URL) with the bare source SRC: refs, objects, fsck of a bare clone.
compare_whole() {
  local name=$1 url=$2 src=$3 back="$OUT/$1-back.git" t
  if diff <(refs_local "$src") <(refs_remote "$url") > "$OUT/$name-refs.diff"; then
    ok "$name: every ref of the source, at the same object ($(refs_local "$src" | wc -l | tr -d ' ') refs)"
  else
    red "$name: refs differ from the source ($OUT/$name-refs.diff: $(wc -l < "$OUT/$name-refs.diff" | tr -d ' ') lines)"
  fi
  t=$(now)
  if bounded "${G[@]}" clone -q --bare "$url" "$back" 2> "$OUT/$name-clone.err"; then
    ok "$name: bare clone from slates ($(perl -e 'printf "%.1f", $ARGV[1]-$ARGV[0]' "$t" "$(now)") s)"
  else
    red "$name: bare clone from slates failed: $(tail -3 "$OUT/$name-clone.err" | tr '\n' ' ')"
    return
  fi
  if git -C "$back" fsck --full --strict > "$OUT/$name-fsck.out" 2>&1; then
    ok "$name: fsck --full --strict of the clone"
  else
    red "$name: fsck of the clone: $(grep -vE '^(Checking|checking)' "$OUT/$name-fsck.out" | head -5 | tr '\n' ' ')"
  fi
  if diff <(objects "$src") <(objects "$back") > "$OUT/$name-objects.diff"; then
    ok "$name: the clone holds every object reachable in the source and no other ($(objects "$src" | wc -l | tr -d ' '))"
  else
    red "$name: reachable objects differ ($OUT/$name-objects.diff: $(wc -l < "$OUT/$name-objects.diff" | tr -d ' ') lines)"
  fi
  if diff <(refs_local "$src") <(refs_local "$back") > /dev/null; then
    ok "$name: the clone's refs are the source's"
  else
    red "$name: the clone's refs differ from the source's"
  fi
}

# Checks out BRANCH of URL and of the source SRC under core.autocrlf AUTOCRLF and compares the two trees byte for byte.
compare_checkout() {
  local name=$1 url=$2 src=$3 branch=$4 autocrlf=$5 a="$OUT/$1-wt-slates-$5" b="$OUT/$1-wt-source-$5"
  if ! bounded "${G[@]}" -c core.autocrlf="$autocrlf" clone -q -b "$branch" "$url" "$a" 2> "$OUT/$name-wt.err"; then
    red "$name: checkout of $branch from slates (autocrlf=$autocrlf): $(tail -3 "$OUT/$name-wt.err" | tr '\n' ' ')"
    return
  fi
  git -c core.autocrlf="$autocrlf" clone -q -b "$branch" "file://$src" "$b" 2> /dev/null
  if diff -r --exclude=.git "$a" "$b" > "$OUT/$name-wt-$autocrlf.diff" 2>&1; then
    ok "$name: $branch checked out byte-identical to a clone of the source (autocrlf=$autocrlf, $(cd "$a" && git ls-files | wc -l | tr -d ' ') files)"
  else
    red "$name: $branch's checked-out bytes differ (autocrlf=$autocrlf; $OUT/$name-wt-$autocrlf.diff)"
  fi
  if [ -z "$(git -C "$a" status --porcelain)" ]; then
    ok "$name: git status clean in the checkout from slates (autocrlf=$autocrlf)"
  else
    red "$name: git status not clean in the checkout from slates (autocrlf=$autocrlf): $(git -C "$a" status --porcelain | head -3 | tr '\n' ' ')"
  fi
}

# A second clone pushes a commit and an annotated tag; the first pulls; then the tag is deleted by push.
pull_and_tags() {
  local name=$1 url=$2 branch=$3 one="$OUT/$1-one" two="$OUT/$1-two"
  bounded "${G[@]}" clone -q -b "$branch" --no-checkout "$url" "$one" 2> /dev/null &&
    bounded "${G[@]}" clone -q -b "$branch" --no-checkout "$url" "$two" 2> /dev/null || { red "$name: the two clones for the pull"; return; }
  local tree head
  tree=$(git -C "$two" rev-parse "HEAD^{tree}") && head=$(git -C "$two" commit-tree "$tree" -p HEAD -m "realtools pull")
  git -C "$two" tag -a realtools-tag -m "an annotated tag" "$head"
  if bounded "${G[@]}" -C "$two" push -q origin "$head:refs/heads/$branch" realtools-tag 2> "$OUT/$name-push.err"; then
    ok "$name: a commit and an annotated tag pushed"
  else
    red "$name: push of a commit and a tag: $(tail -3 "$OUT/$name-push.err" | tr '\n' ' ')"
    return
  fi
  bounded "${G[@]}" -C "$one" fetch -q --tags origin 2> /dev/null
  if [ "$(git -C "$one" rev-parse "origin/$branch")" = "$head" ] &&
    [ "$(git -C "$one" rev-parse realtools-tag)" = "$(git -C "$two" rev-parse realtools-tag)" ]; then
    ok "$name: the pull sees the new commit and the annotated tag, the tag object's id unchanged"
  else
    red "$name: the pull's commit or tag differs from what was pushed"
  fi
  bounded "${G[@]}" -C "$two" push -q origin :refs/tags/realtools-tag 2> /dev/null
  if refs_remote "$url" | grep -q ' refs/tags/realtools-tag$'; then
    red "$name: a tag deleted by push is still listed"
  else
    ok "$name: a tag deleted by push is gone"
  fi
}

# Pushes the bare source SRC whole to a new slates repository NAME and runs every comparison.
round() {
  local name=$1 src=$2 branch=$3 t url="http://127.0.0.1:$port/$1"
  "$SLATES" --instance "$instance" repository create "$name" --tool git --default-branch "$branch" > /dev/null ||
    { red "$name: repository create"; return; }
  t=$(now)
  if bounded "${G[@]}" -C "$src" push -q "$url" 'refs/heads/*:refs/heads/*' 'refs/tags/*:refs/tags/*' 2> "$OUT/$name-first-push.err"; then
    ok "$name: the whole history pushed ($(perl -e 'printf "%.1f", $ARGV[1]-$ARGV[0]' "$t" "$(now)") s)"
  else
    red "$name: the first push failed: $(tail -3 "$OUT/$name-first-push.err" | tr '\n' ' ')"
    return
  fi
  compare_whole "$name" "$url" "$src"
  compare_checkout "$name" "$url" "$src" "$branch" true
  compare_checkout "$name" "$url" "$src" "$branch" false
  pull_and_tags "$name" "$url" "$branch"
}

# The Windows-paths repository, made with plumbing so that names Windows refuses on its disk are in history anyway.
# Branch main: checkout-safe names that stress Windows (CRLF attributes, a path past 260 characters, names that differ
# from each other only by Unicode normal form). Branch hostile: names a Windows checkout refuses (reserved device
# names, case-colliding names, a trailing dot), held as tree entries only, compared as objects.
paths_source() {
  local src="$OUT/paths-src.git" work="$OUT/paths-build"
  git init -q --bare "$src" && git init -q "$work" || return 1
  local idx="$work/.git/index" long="deep"
  for _ in $(seq 1 30); do long="$long/abcdefghij"; done
  add() { # PATH CONTENT(printf format)
    local id
    id=$(printf "$2" | git -C "$work" hash-object -w --stdin)
    GIT_INDEX_FILE=$idx git -C "$work" update-index --add --cacheinfo "100644,$id,$1"
  }
  printf '*.txt text eol=crlf\n*.sh text eol=lf\n*.bin binary\n*.auto text=auto\n' > "$work/.ga"
  add .gitattributes "$(cat "$work/.ga")\n"
  add crlf.txt 'one\ntwo\nthree\n'
  add lf.sh '#!/bin/sh\necho lf\n'
  add committed-crlf.auto 'crlf in the blob\r\nsecond\r\n'
  add mixed.auto 'lf line\ncrlf line\r\nlf again\n'
  add bytes.bin '\000\001\r\n\n\r\377'
  add "$long/past-260.txt" 'a path longer than MAX_PATH\n'
  add "unicode/caf$(printf '\303\251').txt" 'NFC e-acute\n'
  add "unicode/cafe$(printf '\314\201')-nfd.txt" 'NFD e plus combining acute\n'
  add "spaces and (parens)/x y.txt" 'spaces\n'
  local tree main
  tree=$(GIT_INDEX_FILE=$idx git -C "$work" write-tree)
  main=$(git -C "$work" commit-tree "$tree" -m "windows paths: checkout-safe")
  add CON.txt 'reserved: CON\n'
  add aux.c 'reserved: aux\n'
  add com1/inner.txt 'reserved directory com1\n'
  add nul 'reserved: nul\n'
  add Readme.md 'case one\n'
  add README.md 'case two\n'
  add "trailing.dot." 'trailing dot\n'
  tree=$(GIT_INDEX_FILE=$idx git -C "$work" write-tree)
  local hostile
  hostile=$(git -C "$work" commit-tree "$tree" -p "$main" -m "windows paths: names Windows refuses on disk")
  git -C "$work" push -q "$src" "$main:refs/heads/main" "$hostile:refs/heads/hostile" || return 1
  git -C "$src" tag lightweight-tag "$main" && git -C "$src" tag -a annotated-tag -m "annotated" "$hostile"
  echo "$src"
}

src=$(paths_source) || { red "the Windows-paths source could not be made"; exit 1; }
round paths "$src" main
if [ -n "$GITGIT" ]; then
  round gitgit "$GITGIT" "$(git -C "$GITGIT" symbolic-ref --short HEAD)"
fi
echo "result: $oks OK, $reds RED (out: $OUT)"
[ "$reds" -eq 0 ]
