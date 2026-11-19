#!/usr/bin/env bash
# Makes slates-hg's import golden vectors (A-630 parts 2 and 3) with stock Mercurial 7.2, from PyPI through uv (nothing
# installed system-wide). Fixed users and dates, fixed docket ids (7.2's own test hook, HGTEST_UUIDFILE, one seed file a
# repository) and a fixed garbage timestamp for the file index. Usage: make-import-fixtures.sh OUT STORE
# STORE is fixtures/store (A-630 part 1): two stores of this same history, and every revision's `hg debugdata`, which
# are this history's golden texts here too.
# One history (fixtures/store's, made by the same steps), made in five repositories:
#   dot        7.2's default (share-safe; fncache with dotencode): its store compared file by file with STORE/dot
#   hybrid     `format.dotencode=False`: compared with STORE/hybrid
#   legacy     `format.use-share-safe=False`: every requirement in .hg/requires and no store/requires; compared with
#              STORE/dot
#   v2         `experimental.revlogv2` and `format.exp-use-changelog-v2`: every revlog behind a docket (the changelog's
#              entries its own), the data revlogs listed by the file index (`fileindex-v1`), named by the dotencode
#              encoding though `dotencode` is not required (7.2's `localrepo.makestore`)
#   fileindex  `format.use-fileindex-v1`: version 1 revlogs listed by the file index
# Both file index stores need `storage.fileindex.slow-path=allow`, since the PyPI build has no Rust file index.
# For each repository R:
#   R-hg-requires.txt          .hg/requires
# and for v2 and fileindex:
#   R/<path>                   every file of .hg/store as stored (but `undo*`, `*.bck` and `phaseroots`)
#   R-files.txt                the store's files, one per line, sorted
#   R-requires.txt             the store's requirements (store/requires)
#   R-fileindex.txt            `hg debug::file-index`: each token and its path
#   R-fileindex-docket.txt     `hg debug::file-index --docket`
# Every revision of v2's and fileindex's changelog, manifest log and file logs (`hg debugdata`) is compared byte for byte
# with STORE's texts (`cmp`), and their path lists with STORE/dot-paths.txt; the script stops on a difference. Last it
# prints every file's SHA-256 and writes them to SHA256SUMS.
set -euo pipefail
OUT=$(cd "$1" && pwd)
STORE=$(cd "$2" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
WORK=$(mktemp -d "$HOME/.cache/g300-importfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
EXTRA=()
hg() { uvx --quiet --python 3.12 --from mercurial==7.2 hg --config ui.username='Test <test@example.com>' \
  --config devel.default-date='0 0' --config storage.fileindex.slow-path=allow \
  --config devel.fileindex.garbage-timestamp=0 ${EXTRA[@]+"${EXTRA[@]}"} "$@"; }
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
stored() {
  (cd "$WORK/$1/.hg/store" && find . -type f ! -name 'undo*' ! -name '*.bck' ! -name 'phaseroots' \
    | sed 's|^\./||' | LC_ALL=C sort)
}
same_store() {
  local repo=$1 golden=$2 file
  stored "$repo" > "$WORK/$repo-files"
  grep -v '^requires$' "$WORK/$repo-files" > "$WORK/$repo-revlogs" || true
  grep -v '^requires$' "$STORE/$golden-files.txt" > "$WORK/$golden-revlogs" || true
  cmp "$WORK/$repo-revlogs" "$WORK/$golden-revlogs"
  while IFS= read -r file; do cmp "$WORK/$repo/.hg/store/$file" "$STORE/$golden/$file"; done < "$WORK/$repo-revlogs"
  echo "$repo: every revlog and the fncache byte for byte $golden's in STORE"
}
dump() {
  local repo=$1 name=$2 rev=0 count
  shift 2
  count=$( (cd "$WORK/$repo" && hg debugindex "$@") | tail -n +2 | wc -l | tr -d ' ')
  while [ "$rev" -lt "$count" ]; do
    (cd "$WORK/$repo" && hg debugdata "$@" "$rev") > "$WORK/text"
    cmp "$WORK/text" "$STORE/$name-$rev.txt"
    rev=$((rev + 1))
  done
  [ -e "$STORE/$name-$rev.txt" ] && { echo "$repo $name: fewer revisions than STORE" >&2; exit 1; }
  return 0
}
hg version -q
for repo in dot hybrid legacy v2 fileindex; do
  case $repo in
    dot) EXTRA=() ;;
    hybrid) EXTRA=(--config format.dotencode=False) ;;
    legacy) EXTRA=(--config format.use-share-safe=False) ;;
    v2) EXTRA=(--config experimental.revlogv2=enable-unstable-format-and-corrupt-my-data
      --config format.exp-use-changelog-v2=enable-unstable-format-and-corrupt-my-data) ;;
    fileindex) EXTRA=(--config format.use-fileindex-v1=yes) ;;
  esac
  export HGTEST_UUIDFILE="$WORK/uuid-$repo"
  hg init "$WORK/$repo"
  (cd "$WORK/$repo" && history)
  EXTRA=()
  cp "$WORK/$repo/.hg/requires" "$OUT/$repo-hg-requires.txt"
  case $repo in
    dot|hybrid) same_store "$repo" "$repo"; cmp "$WORK/$repo/.hg/store/requires" "$STORE/$repo-requires.txt" ;;
    legacy)
      same_store legacy dot
      if [ -e "$WORK/legacy/.hg/store/requires" ]; then echo "legacy: a store/requires exists" >&2; exit 1; fi ;;
    v2|fileindex)
      stored "$repo" > "$OUT/$repo-files.txt"
      while IFS= read -r file; do
        mkdir -p "$OUT/$repo/$(dirname "$file")"
        cp "$WORK/$repo/.hg/store/$file" "$OUT/$repo/$file"
      done < "$OUT/$repo-files.txt"
      cp "$WORK/$repo/.hg/store/requires" "$OUT/$repo-requires.txt"
      (cd "$WORK/$repo" && hg debug::file-index) > "$OUT/$repo-fileindex.txt"
      (cd "$WORK/$repo" && hg debug::file-index --docket) > "$OUT/$repo-fileindex-docket.txt"
      (cd "$WORK/$repo" && hg manifest --all) | while IFS= read -r path; do
        printf '%s' "$path" | xxd -p | tr -d '\n'; echo
      done > "$WORK/$repo-paths"
      cmp "$WORK/$repo-paths" "$STORE/dot-paths.txt"
      dump "$repo" 00changelog -c
      dump "$repo" 00manifest -m
      n=0
      while IFS= read -r path; do
        dump "$repo" "f$n" "$(printf '%s' "$path" | xxd -r -p)"
        n=$((n + 1))
      done < "$STORE/dot-paths.txt"
      echo "$repo: every revision's debugdata byte for byte STORE's" ;;
  esac
done
cd "$OUT"
find . -type f ! -name '*.sh' ! -name SHA256SUMS | sed 's|^\./||' | LC_ALL=C sort \
  | while IFS= read -r file; do shasum -a 256 -- "$file"; done | tee SHA256SUMS
