#!/usr/bin/env bash
# Makes slates-hg's golden vectors for a repository's state beside its history (A-649 part 2): its phases, bookmarks
# and obsolescence markers, with stock Mercurial 7.2 from PyPI through uv (nothing installed system-wide). Fixed users,
# dates and docket ids (7.2's HGTEST_UUIDFILE). Usage: make-state-fixtures.sh OUT
# One history in two repositories: `states` (bookmarks in `.hg/bookmarks`, 7.2's default) and `states-store`
# (`format.bookmarks-in-store`: the store's own `bookmarks`, required as `bookmarksinstore`). The history, made with
# evolution on and publishing off: a root made public, a draft line, a draft amended (a marker from the old changeset to
# the new, the old one hidden), a secret changeset, a bookmark on each of three changesets (the secret one's among them)
# and a prune marker for a node the repository never held.
# For each repository R:
#   R/<path>            every file of .hg/store as stored (but `undo*` and `*.bck`): `phaseroots`, `obsstore` and, in
#                       states-store, `bookmarks` among them
#   R-files.txt         the store's files, sorted;   R-requires.txt  store/requires;   R-hg-requires.txt  .hg/requires
#   R-bookmarks         .hg/bookmarks as stored, where the repository keeps it there
#   R-phases.txt        `hg log --hidden -T '{node} {phase}\n'`: every changeset's phase as 7.2 computes it
#   R-bookmarks.txt     `hg bookmarks -T '{bookmark} {node}\n'`
#   R-markers.txt       `hg debugobsolete`: every marker as 7.2 lists it, in the obsstore's order
# Last it prints every file's SHA-256 and writes them to SHA256SUMS.
set -euo pipefail
OUT=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
WORK=$(mktemp -d "$HOME/.cache/g300-statefix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
EXTRA=()
hg() { uvx --quiet --python 3.12 --from mercurial==7.2 hg --config ui.username='Test <test@example.com>' \
  --config devel.default-date='0 0' --config experimental.evolution=all --config phases.publish=False \
  ${EXTRA[@]+"${EXTRA[@]}"} "$@"; }
history() {
  printf 'a\n' > a; hg add -q a; hg commit -q -m 'public root'
  hg phase -q --public -r .
  printf 'b\n' > b; hg add -q b; hg commit -q -m 'draft one'
  hg bookmark -q -r . one
  printf 'c\n' > c; hg add -q c; hg commit -q -m 'draft two'
  printf 'c amended\n' > c; hg commit -q --amend -m 'draft two, amended'
  hg bookmark -q -r . two
  printf 'd\n' > d; hg add -q d; hg commit -q --secret -m 'secret'
  hg bookmark -q -r . secret-one
  hg debugobsolete -q 1111111111111111111111111111111111111111
}
hg version -q
for repo in states states-store; do
  case $repo in
    states) EXTRA=() ;;
    states-store) EXTRA=(--config format.bookmarks-in-store=yes) ;;
  esac
  export HGTEST_UUIDFILE="$WORK/uuid-$repo"
  hg init "$WORK/$repo"
  (cd "$WORK/$repo" && history)
  EXTRA=()
  (cd "$WORK/$repo/.hg/store" && find . -type f ! -name 'undo*' ! -name '*.bck' | sed 's|^\./||' | LC_ALL=C sort) \
    > "$OUT/$repo-files.txt"
  while IFS= read -r file; do
    mkdir -p "$OUT/$repo/$(dirname "$file")"
    cp "$WORK/$repo/.hg/store/$file" "$OUT/$repo/$file"
  done < "$OUT/$repo-files.txt"
  cp "$WORK/$repo/.hg/store/requires" "$OUT/$repo-requires.txt"
  cp "$WORK/$repo/.hg/requires" "$OUT/$repo-hg-requires.txt"
  if [ -e "$WORK/$repo/.hg/bookmarks" ]; then cp "$WORK/$repo/.hg/bookmarks" "$OUT/$repo-bookmarks"; fi
  (cd "$WORK/$repo" && hg log --hidden -T '{node} {phase}\n') > "$OUT/$repo-phases.txt"
  (cd "$WORK/$repo" && hg bookmarks -T '{bookmark} {node}\n') > "$OUT/$repo-bookmarks.txt"
  (cd "$WORK/$repo" && hg debugobsolete) > "$OUT/$repo-markers.txt"
done
cd "$OUT"
find . -type f ! -name '*.sh' ! -name SHA256SUMS | sed 's|^\./||' | LC_ALL=C sort \
  | while IFS= read -r file; do shasum -a 256 -- "$file"; done | tee SHA256SUMS
