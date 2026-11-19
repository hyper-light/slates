#!/usr/bin/env bash
# Makes slates-hg's store golden vectors (A-630) with stock Mercurial 7.2, from PyPI through uv (nothing installed
# system-wide). Fixed users and dates. Usage: make-store-fixtures.sh OUT
# One history, made in two repositories: `dot` (7.2's default store: fncache with dotencode) and `hybrid`
# (`format.dotencode=False`: fncache with the hybrid encoding alone). Its file names take every branch of 7.2's store
# encoding (`store.py`): capitals and underscores, case-colliding names (`FOO`, removed, then `foo`), Windows' reserved
# names (`aux.txt`, `con`, `com1`, `lpt9.x`, `prn.d`), reserved bytes (`:`, `?`, `*`, `"`, `<`, `>`, `|`, `\`, `~`,
# `é`), a leading period and a leading space, a trailing period and a trailing space, directories named `x.i`, `y.d`
# and `z.hg` (encodedir), a path over 120 bytes (hashed whole), a path short enough raw but over 120 encoded (hashed
# after encoding), and deep directories (hashed with their prefixes, one ending in a period).
# For each repository R:
#   R/<path>            every file of `.hg/store`, as stored (each name is the store's own encoding, Windows-safe)
#   R-files.txt         the store's files, one per line, sorted
#   R-requires.txt      the store's requirements
#   R-paths.txt         every tracked path the history ever held, hexadecimal, one per line (`hg manifest --all`)
# And, once (the two histories are the same): <log>-<rev>.txt `hg debugdata` of every revision of the changelog,
# the manifest log and each file's log (`f<n>` for the n-th path of R-paths.txt), every other repository's texts
# compared with these byte for byte (`cmp`, the script stops on a difference). Last it prints every file's SHA-256.
set -euo pipefail
OUT=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
WORK=$(mktemp -d "$HOME/.cache/g291-storefix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
EXTRA=()
hg() { uvx --quiet --python 3.12 --from mercurial==7.2 hg --config ui.username='Test <test@example.com>' \
  --config devel.default-date='0 0' ${EXTRA[@]+"${EXTRA[@]}"} "$@"; }
LONG=$(printf 'a%.0s' $(seq 1 130))
ENCODED_LONG="UPPER_$(printf 'Z%.0s' $(seq 1 50))"
DEEP="directory.one/seventh.directory/third.directory/fourth.dir./fifth.directory/sixth.directory/eighth.directory/ninth.directory"
names() {
  printf '%s\n' 'plain.txt' 'Capital_Under.txt' 'aux.txt' 'con' 'com1' 'lpt9.x' 'prn.d' \
    'colon:quest?star*.txt' 'quote"lt<gt>pipe|back\slash.txt' 'tilde~é.txt' '.leading' ' space' \
    'trailing.' 'trailing ' 'x.i/inside.txt' 'y.d/inside.txt' 'z.hg/inside.txt' "$LONG.txt" \
    "$ENCODED_LONG.txt" "$DEEP/deep-file-name.txt"
}
history() {
  printf 'first\n' > FOO; hg add -q FOO; hg commit -q -m 'add FOO'
  hg rm -q FOO; hg commit -q -m 'remove FOO'
  printf 'second\n' > foo; hg add -q foo; hg commit -q -m 'add foo'
  while IFS= read -r name; do
    mkdir -p "$(dirname "$name")"
    printf 'the file %s\n' "$name" > "$name"
  done < <(names)
  hg add -q .; hg commit -q -m 'every encoding'
  printf 'changed\n' >> plain.txt; printf 'changed\n' >> "$LONG.txt"; hg commit -q -m 'change two'
}
dump() {
  local repo=$1 name=$2 rev=0 count
  shift 2
  count=$( (cd "$WORK/$repo" && hg debugindex "$@") | tail -n +2 | wc -l | tr -d ' ')
  while [ "$rev" -lt "$count" ]; do
    (cd "$WORK/$repo" && hg debugdata "$@" "$rev") > "$WORK/text"
    if [ "$repo" = dot ]; then cp "$WORK/text" "$OUT/$name-$rev.txt"; else cmp "$WORK/text" "$OUT/$name-$rev.txt"; fi
    rev=$((rev + 1))
  done
}
hg version -q
for repo in dot hybrid; do
  case $repo in
    dot) EXTRA=() ;;
    hybrid) EXTRA=(--config format.dotencode=False) ;;
  esac
  hg init "$WORK/$repo"
  (cd "$WORK/$repo" && history)
  EXTRA=()
  (cd "$WORK/$repo/.hg/store" && find . -type f ! -name 'undo*' ! -name '*.bck' ! -name 'phaseroots' \
    | sed 's|^\./||' | LC_ALL=C sort) > "$OUT/$repo-files.txt"
  while IFS= read -r file; do
    mkdir -p "$OUT/$repo/$(dirname "$file")"
    cp "$WORK/$repo/.hg/store/$file" "$OUT/$repo/$file"
  done < "$OUT/$repo-files.txt"
  cp "$WORK/$repo/.hg/store/requires" "$OUT/$repo-requires.txt"
  (cd "$WORK/$repo" && hg manifest --all) > "$WORK/paths"
  (cd "$WORK/$repo" && hg manifest --all) | while IFS= read -r path; do printf '%s' "$path" | xxd -p | tr -d '\n'; echo; done \
    > "$OUT/$repo-paths.txt"
  dump "$repo" 00changelog -c
  dump "$repo" 00manifest -m
  n=0
  while IFS= read -r path; do dump "$repo" "f$n" "$path"; n=$((n + 1)); done < "$WORK/paths"
done
cmp "$OUT/dot-paths.txt" "$OUT/hybrid-paths.txt"
cd "$OUT"
find . -type f ! -name '*.sh' | sed 's|^\./||' | LC_ALL=C sort | while IFS= read -r file; do shasum -a 256 -- "$file"; done
