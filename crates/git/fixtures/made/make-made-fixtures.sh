#!/usr/bin/env bash
# Makes slates-git's golden vectors for the trees and commits the server makes (A-664; §4.17.2 "Pieces (git)" piece
# 13) with stock git, in a SHA-1 and a SHA-256 repository:
# - trees: each row of files.tsv put in the index by `git update-index --add --cacheinfo <mode>,<id>,<path>` (each
#   blob written by `git hash-object -w --stdin` from the row's bytes, a submodule link by its commit id), the root by
#   `git write-tree`, every tree beneath it by `git ls-tree -r -t -z`, and the empty tree by `git mktree`. One line a
#   tree in OUTDIR/<format>/trees.txt: `<id> <path> <bytes>`, the bytes as `git cat-file tree` prints them, in
#   hexadecimal; the root's path is `.`, the empty tree's `-`.
# - commits: each row of commits.tsv by `git commit-tree <tree> [-p <parent>]... -m <message>`, the row's name and
#   email as author and committer (an email `-` is none), both at its time in UTC (`GIT_AUTHOR_DATE='<seconds> +0000'`); `-m` completes the
#   message's last line, and an empty one is read from an empty stdin, as commit-tree reads it. One line a commit in
#   OUTDIR/<format>/commits.txt: `<case> <id> <bytes>`, the bytes as `git cat-file commit` prints them, in hexadecimal.
# Then OUTDIR/SHA256SUMS, every output file's SHA-256, which the tests check.
#
# Usage: make-made-fixtures.sh OUTDIR (OUTDIR must hold no sha1/ or sha256/ yet). The inputs are read from beside this
# script. Deterministic: no configuration but this script's, every identity, date and byte the inputs'. A second run
# writes the same bytes.
#
# Sources (git v2.50.1): git-update-index (--cacheinfo), git-write-tree, git-ls-tree, git-mktree, git-commit-tree;
# builtin/commit-tree.c (-m completes the line; an empty message is read from stdin), commit.c (commit_tree_extended),
# ident.c (fmt_ident).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(cd "$1" && pwd)
if [ -e "$OUT/sha1" ] || [ -e "$OUT/sha256" ]; then
  echo "make-made-fixtures.sh: $OUT already holds sha1/ or sha256/; give it an empty directory" >&2
  exit 1
fi
unset GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 TZ=UTC LC_ALL=C LANG=C
WORK=$(mktemp -d "$HOME/.cache/slates-git-madefix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"
mkdir -p "$HOME"
git --version > "$OUT/git-version.txt"

# The bytes a hexadecimal string spells, on stdout.
bytes_of() { perl -e 'binmode STDOUT; print pack("H*", $ARGV[0])' "$1"; }
# Stdin's bytes as lower-case hexadecimal, on stdout.
hex_of() { perl -e 'binmode STDIN; local $/; my $in = <STDIN>; print unpack("H*", defined $in ? $in : "")'; }
# The id the line of FILE whose column KEY_COLUMN is KEY holds in column VALUE_COLUMN.
lookup() { awk -v key="$2" -v k="$3" -v v="$4" '$k == key { print $v; exit }' "$1"; }

for format in sha1 sha256; do
  mkdir -p "$OUT/$format"
  repo="$WORK/$format"
  git init -q --object-format="$format" "$repo"
  (
    cd "$repo"
    while IFS=$'\t' read -r formats mode path hex; do
      case "$formats" in
        "#"*) continue ;;
        both | "$format") ;;
        *) continue ;;
      esac
      if [ "$mode" = 160000 ]; then
        id=$hex
      else
        id=$(bytes_of "$hex" | git hash-object -w --stdin)
      fi
      git update-index --add --cacheinfo "$mode,$id,$path"
    done < "$HERE/files.tsv"
    root=$(git write-tree)
    empty=$(git mktree < /dev/null)
    trees="$OUT/$format/trees.txt"
    printf '%s %s %s\n' "$root" . "$(git cat-file tree "$root" | hex_of)" > "$trees"
    git ls-tree -r -t -z --full-tree "$root" | while IFS= read -r -d '' line; do
      meta=${line%%$'\t'*}
      path=${line#*$'\t'}
      set -- $meta
      if [ "$2" = tree ]; then
        printf '%s %s %s\n' "$3" "$path" "$(git cat-file tree "$3" | hex_of)" >> "$trees"
      fi
    done
    printf '%s %s %s\n' "$empty" - "" >> "$trees"
    commits="$OUT/$format/commits.txt"
    : > "$commits"
    while IFS=$'\t' read -r case tree parents seconds name email hex; do
      case "$case" in "#"*) continue ;; esac
      tree_id=$(lookup "$trees" "$tree" 2 1)
      args=()
      if [ "$parents" != - ]; then
        for parent in ${parents//,/ }; do
          args+=(-p "$(lookup "$commits" "$parent" 1 2)")
        done
      fi
      if [ "$email" = - ]; then email=; fi
      message=$(bytes_of "$hex"; printf x)
      message=${message%x}
      id=$(GIT_AUTHOR_NAME="$name" GIT_AUTHOR_EMAIL="$email" GIT_AUTHOR_DATE="$seconds +0000" \
        GIT_COMMITTER_NAME="$name" GIT_COMMITTER_EMAIL="$email" GIT_COMMITTER_DATE="$seconds +0000" \
        git commit-tree "$tree_id" ${args[@]+"${args[@]}"} -m "$message" < /dev/null)
      printf '%s %s %s\n' "$case" "$id" "$(git cat-file commit "$id" | hex_of)" >> "$commits"
    done < "$HERE/commits.tsv"
    # Every object stock git made here passes its own strict check, as a fetching client's would.
    git fsck --strict --no-dangling --no-progress
  )
done
(cd "$OUT" && shasum -a 256 git-version.txt sha1/trees.txt sha1/commits.txt sha256/trees.txt sha256/commits.txt) \
  > "$OUT/SHA256SUMS"
