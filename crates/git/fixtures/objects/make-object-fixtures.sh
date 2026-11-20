#!/usr/bin/env bash
# Makes slates-git's object-name golden vectors (A-600) with stock git. In a SHA-1 repository and a SHA-256 one:
# - one object of each type, its content as `git cat-file <type>` prints it raw;
# - its name as `git hash-object -t <type> --stdin` computes it from that content, which must equal the name
#   `git rev-parse` reads from the repository;
# - the empty blob's name.
# Then the SHAttered pair: git's own t0013 vector (`t/t0013/shattered-1.pdf` at v2.50.1) and its twin from shattered.io,
# each checked by its SHA-256 before it is cut to the 320 bytes that hold both colliding blocks. The two differ at bytes
# 192 to 319 and nowhere after, so the cuts collide under plain SHA-1 as the whole files do.
# Deterministic: no configuration but this script's, a fixed identity, every date 2005-04-07T22:13:13Z.
# Usage: make-object-fixtures.sh OUTDIR
#
# Sources: git's object-file.c (format_object_header), hash.c (hash_algos), t/t0013-sha1dc.sh; shattered.io.
set -euo pipefail
OUT=$(cd "$1" && pwd)
unset GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 TZ=UTC LC_ALL=C LANG=C
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_AUTHOR_DATE='1112911993 +0000' GIT_COMMITTER_DATE='1112911993 +0000'
WORK=$(mktemp -d "$HOME/.cache/slates-git-objfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"; mkdir -p "$HOME"
git --version > "$OUT/git-version.txt"
for format in sha1 sha256; do
  mkdir -p "$OUT/$format"
  repo="$WORK/$format"
  git init -q -b main --object-format="$format" "$repo"
  (
    cd "$repo"
    git config commit.gpgsign false; git config tag.gpgsign false
    printf 'the blob both repositories hold\n' > file
    mkdir dir; printf 'a file in a directory\n' > dir/inner
    git add file dir/inner; git commit -q -m 'the commit both repositories hold'
    git tag -a -m 'the tag both repositories hold' v1
    : > "$OUT/$format/names.txt"
    for spec in "blob HEAD:file" "tree HEAD^{tree}" "commit HEAD" "tag v1"; do
      set -- $spec
      type=$1; name=$(git rev-parse "$2")
      git cat-file "$type" "$name" > "$OUT/$format/$type.bin"
      hashed=$(git hash-object -t "$type" --stdin < "$OUT/$format/$type.bin")
      test "$hashed" = "$name"
      echo "$type $hashed" >> "$OUT/$format/names.txt"
    done
    echo "empty $(git hash-object -t blob --stdin < /dev/null)" >> "$OUT/$format/names.txt"
  )
done
cut_checked() {
  curl -fsSL -o "$WORK/$2.pdf" "$1"
  echo "$3  $WORK/$2.pdf" | shasum -a 256 -c - > /dev/null
  head -c 320 "$WORK/$2.pdf" > "$OUT/$2.prefix.bin"
}
cut_checked https://raw.githubusercontent.com/git/git/v2.50.1/t/t0013/shattered-1.pdf shattered-1 \
  2bb787a73e37352f92383abe7e2902936d1059ad9f1ba6daaa9c1e58ee6970d0
cut_checked https://shattered.io/static/shattered-2.pdf shattered-2 \
  d4488775d29bdef7993367d541064dbdda50d383f89f0aa13a6ff2e0894ba5ff
