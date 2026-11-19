#!/usr/bin/env bash
# Records what stock Mercurial 7.2 serves of the state fixtures' two repositories (A-650's served view): each made again
# from its committed store, `.hg/requires` and `.hg/bookmarks`, so its nodes are the fixtures' own, opened with the
# configuration it was made under (evolution on, publishing off), and read through 7.2's `served` filter, the one
# `hg serve` answers a peer from (`wireprotov1server`). With stock Mercurial 7.2 from PyPI through uv (nothing installed
# system-wide). Usage: make-served-fixtures.sh DIR [REPO...] (DIR this script's directory), every repository
# make-state-fixtures.sh makes when none is named.
# For each repository R:
#   R-served.txt            every changeset the filter serves, in revision order: `{node} {phase}`
#   R-served-heads.txt      the served heads as `heads` answers them, in 7.2's order
#   R-served-branchmap.txt  each branch and its heads, closed ones too, as `branchmap` answers them (7.2's
#                           `wireprotov1server.branchmap`), by branch: `{branch} {node} ...`
#   R-served-bookmarks.txt  `listkeys bookmarks` as a peer is answered, sorted: `{name} {node}`
#   R-served-phases.txt     `listkeys phases` as a peer is answered, sorted: `{key} {value}`
#   R-unserved.txt          every changeset the filter leaves out, in revision order, and why: `{node} hidden` for one
#                           the `visible` filter hides (obsolete and not pinned), `{node} secret` for the rest
# Last it prints every file's SHA-256 and writes them to SHA256SUMS, as make-state-fixtures.sh does.
set -euo pipefail
DIR=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
WORK=$(mktemp -d "$HOME/.cache/g303-servedfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
cat > "$WORK/served.py" <<'PY'
import sys
from mercurial import hg, pushkey, repoview, ui as uimod
from mercurial.node import hex

path, out = sys.argv[1].encode(), sys.argv[2]
u = uimod.ui.load()
u.setconfig(b'experimental', b'evolution', b'all')
u.setconfig(b'phases', b'publish', b'False')
repo = hg.repository(u, path)
served = repo.filtered(b'served')
hidden = repoview.filterrevs(repo, b'visible')


def write(name, lines):
    with open('%s-%s.txt' % (out, name), 'w') as f:
        for line in lines:
            f.write(line + '\n')


def text(value):
    return value.decode('utf-8', 'surrogateescape')


write('served', ['%s %s' % (text(served[r].hex()), text(served[r].phasestr())) for r in served])
write('served-heads', [text(hex(h)) for h in served.heads()])
branchmap = served.branchmap()
write('served-branchmap', ['%s %s' % (text(branch), ' '.join(text(hex(n)) for n in
                                                            branchmap.branchheads(branch, closed=True)))
                           for branch in sorted(branchmap)])
write('served-bookmarks', sorted('%s %s' % (text(k), text(v))
                                 for k, v in pushkey.list(served, b'bookmarks').items()))
write('served-phases', sorted('%s %s' % (text(k), text(v))
                              for k, v in pushkey.list(served, b'phases').items()))
unfiltered = repo.unfiltered()
write('unserved', ['%s %s' % (text(unfiltered[r].hex()), 'hidden' if r in hidden else 'secret')
                   for r in unfiltered if r not in served])
PY
REPOS=(states states-store divergent)
if [ $# -gt 1 ]; then shift; REPOS=("$@"); fi
for repo in "${REPOS[@]}"; do
  mkdir -p "$WORK/$repo/.hg/store"
  while IFS= read -r file; do
    mkdir -p "$WORK/$repo/.hg/store/$(dirname "$file")"
    cp "$DIR/$repo/$file" "$WORK/$repo/.hg/store/$file"
  done < "$DIR/$repo-files.txt"
  cp "$DIR/$repo-hg-requires.txt" "$WORK/$repo/.hg/requires"
  if [ -e "$DIR/$repo-bookmarks" ]; then cp "$DIR/$repo-bookmarks" "$WORK/$repo/.hg/bookmarks"; fi
  uv run --quiet --no-project --python 3.12 --with mercurial==7.2 python "$WORK/served.py" "$WORK/$repo" "$DIR/$repo"
done
cd "$DIR"
find . -type f ! -name '*.sh' ! -name SHA256SUMS | sed 's|^\./||' | LC_ALL=C sort \
  | while IFS= read -r file; do shasum -a 256 -- "$file"; done | tee SHA256SUMS
