#!/bin/bash
# Fetch main, merge it into the A-652 part 3 landing branch when main's new commits touch none of the piece's crates,
# resolve the amendment log's end main-first, commit, and print the diff stat against main. Exit 3: main touched a
# crate of the piece (rerun the tests first); exit 4: the merged tree is not main's plus the piece's 20 files.
set -u
cd ~/slates-work/g268-a652-3-land || exit 2
timeout 400 git fetch -q origin || exit 2
if git merge-base --is-ancestor origin/main HEAD; then echo "FASTFORWARD already"; git push origin HEAD:main 2>&1 | tail -2; exit 0; fi
base=$(git merge-base HEAD origin/main)
new=$(git rev-parse --short=9 origin/main)
touched=$(git diff --name-only "$base" origin/main | grep -E "crates/(svn|ipc|api)/|server/src/(vcs_store|repository|verbs)|crates/db/src/repository|crates/merge/src/engine" )
if [ -n "$touched" ] && [ "${ALLOW:-}" != "$touched" ]; then echo "TOUCHED:"; echo "$touched"; exit 3; fi
files=$(git diff --name-only "$base" origin/main | sed 's#^crates/##' | cut -d/ -f1-3 | sort -u | tr '\n' ' ')
git merge --no-edit origin/main > /dev/null 2>&1
python3 - <<'PY'
import pathlib
p = pathlib.Path("docs/wip/SLATES_DESIGN.md")
s = p.read_text()
if "<<<<<<< HEAD\n" in s:
    lines = s.split("\n")
    start = lines.index("<<<<<<< HEAD"); mid = lines.index("=======", start); end = lines.index(">>>>>>> origin/main", mid)
    ours = lines[start+1:mid]; theirs = lines[mid+1:end]
    assert ours[0].startswith("### A-652 part 3") and lines[start-1] == ""
    p.write_text("\n".join(lines[:start] + theirs + [""] + ours + lines[end+1:]))
PY
git add docs/wip/SLATES_DESIGN.md
if grep -rnqE '^(<<<<<<< |=======$|>>>>>>> )' docs crates; then echo "MARKERS LEFT"; exit 4; fi
unresolved=$(git status --short | grep -vE '^(M |A |D |R )' | head -3)
if [ -n "$unresolved" ]; then echo "UNRESOLVED: $unresolved"; exit 4; fi
stat=$(git diff --cached --stat MERGE_HEAD | tail -1)
case "$stat" in *"20 files changed, 1831 insertions(+), 150 deletions(-)"*) ;; *) echo "STAT: $stat"; exit 4;; esac
cat > ~/.local/state/slates-fanout/g268-scripts/a652-3-remerge-msg.txt <<MSG
merge: origin/main ($new) into A-652 part 3 again as it lands, the push having met main's newer commits -- they change ${files}alone, none of the piece's files or the names it calls, so the run at 940d2a10d (kiln r462, macOS arm64, Mac Studio: fmt --check clean; slates-svn commit:: migration:: 94 of 94; slates-ipc wire_order 2 of 2; slates-api 2 of 2; slates-server --lib vcs_store:: repository:: 72 of 72) stands for the merged tree; the amendment log's end merged main's entries first, A-652 part 3 last

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
MSG
git commit -q -F ~/.local/state/slates-fanout/g268-scripts/a652-3-remerge-msg.txt || exit 5
git log --oneline -1 | cut -c1-100
final=$(git diff --stat origin/main HEAD | tail -1)
echo "against main: $final"
case "$final" in *"20 files changed, 1831 insertions(+), 150 deletions(-)"*) ;; *) echo "FINAL STAT: $final"; exit 4;; esac
echo READY
# The stat above is the piece's 20 files alone and no marker is left: push now, before main moves again.
git push origin HEAD:main 2>&1 | tail -2
echo "pushed? $(git ls-remote origin refs/heads/main | cut -c1-12) vs HEAD $(git rev-parse --short=12 HEAD)"
