#!/usr/bin/env bash
# The second repository of hg_correctness.sh (UI goal 93, correctness by use): one stock Mercurial makes, the same
# bytes every time (fixed user, dates and seeded contents), holding what hg's own history has little of:
# - named branches, one of them closed, and merges between them and default (a merge with a conflict resolved by hand);
# - tags (made by `hg tag`, so .hgtags changes too, one tag moved and one removed) and bookmarks (several, one inactive);
# - phases: the tip of one line left draft, one changeset secret (which no push may carry);
# - large files: binary files of 1 MiB, 16 MiB and 64 MiB (seeded random bytes, which no compression shrinks), one of them
#   changed in place and one renamed, and a file of many short lines changed in many places.
#
# Usage: hg_correctness_fixture.sh HG DIR
#   HG   stock Mercurial 7.2's command (a word list is fine)
#   DIR  the repository to make, which must not exist yet
set -euo pipefail

read -r -a HG_CMD <<< "$1"
DIR=$2
[ -e "$DIR" ] && { echo "$DIR exists" >&2; exit 1; }
HOMEDIR=$(mktemp -d "${DIR%/*}/.fixture-home.XXXXXX")
trap 'rm -rf "$HOMEDIR"' EXIT
h() {
  env "HOME=$HOMEDIR" HGRCPATH= HGPLAIN=1 HGUSER="Fixture <fixture@example.com>" HGENCODING=utf-8 LC_ALL=C \
    "${HG_CMD[@]}" --noninteractive -R "$DIR" "$@"
}
# Each commit at its own fixed date, one minute apart.
MINUTE=0
commit() { MINUTE=$((MINUTE + 1)); h commit -q -A -d "$((1700000000 + MINUTE * 60)) 0" -m "$1" "${@:2}"; }
# Seeded random bytes: file, size in MiB, seed.
blob() { python3 -c 'import random,sys; r=random.Random(int(sys.argv[3])); open(sys.argv[1],"wb").write(r.randbytes(int(sys.argv[2])<<20))' "$1" "$2" "$3"; }
lines() { python3 -c 'import sys; n=int(sys.argv[2]); s=int(sys.argv[3]); open(sys.argv[1],"w").write("".join(f"line {i} {(i*s)%9973}\n" for i in range(n)))' "$1" "$2" "$3"; }

mkdir -p "$DIR"
env "HOME=$HOMEDIR" HGRCPATH= "${HG_CMD[@]}" init "$DIR"
cd "$DIR"

lines text.txt 20000 1; echo "readme" > README; commit "the start"
blob big1.bin 1 1; commit "a 1 MiB binary"
blob big16.bin 16 2; commit "a 16 MiB binary"
h tag -q -d "1700001000 0" v0.1
for n in $(seq 1 12); do lines text.txt 20000 $((n + 1)); commit "default change $n"; done
h bookmark -q mainline

# A named branch with its own history, merged into default twice.
h branch -q stable
for n in $(seq 1 6); do echo "stable $n" >> stable.txt; commit "stable change $n"; done
h tag -q -d "1700002000 0" v0.2-stable
h update -q default
for n in $(seq 1 4); do echo "default $n" >> default.txt; commit "default after the branch $n"; done
h merge -q stable; commit "merge stable into default"
h update -q stable
for n in $(seq 7 9); do echo "stable $n" >> stable.txt; commit "stable change $n"; done
h update -q default
h merge -q stable; commit "merge stable into default again"

# A conflict resolved by hand: both lines change the same lines of text.txt.
h update -q stable
lines text.txt 20000 101; commit "stable rewrites text"
h update -q default
lines text.txt 20000 202; commit "default rewrites text"
h merge -q --tool internal:other stable || true
lines text.txt 20000 303; h resolve -q -m text.txt; commit "merge with the conflict resolved by hand"

# Large files changed and renamed; a 64 MiB file.
blob big64.bin 64 3; commit "a 64 MiB binary"
blob big16.bin 16 4; commit "the 16 MiB binary changed in place"
h mv -q big1.bin renamed1.bin; commit "the 1 MiB binary renamed"

# A branch closed, a tag moved and one removed, bookmarks.
h branch -q feature
for n in $(seq 1 3); do echo "feature $n" >> feature.txt; commit "feature change $n"; done
h bookmark -q feature-mark
commit "close feature" --close-branch
h update -q default
h tag -q -f -d "1700003000 0" v0.1
h tag -q -d "1700003100 0" doomed
h tag -q --remove -d "1700003200 0" doomed
h bookmark -q -r 'desc("merge stable into default again")' older-mark
h bookmark -q --inactive mainline

# Phases: everything so far public; a draft line; a secret changeset on top of it.
h phase -q --public -r 'all()'
echo "draft 1" > draft.txt; commit "a draft changeset"
echo "draft 2" >> draft.txt; commit "another draft changeset"
echo "secret" > secret.txt; commit "a secret changeset"
h phase -q --force --secret -r .
h update -q -r 'desc("another draft changeset")'

echo "fixture: $(h log -r 'all()' -T x | wc -c | tr -d ' ') changesets, $(h branches -c -T x | wc -c | tr -d ' ') branches, $(h tags -T x | wc -c | tr -d ' ') tags, $(h bookmarks -T x | wc -c | tr -d ' ') bookmarks, $(h log -r 'draft()' -T x | wc -c | tr -d ' ') draft, $(h log -r 'secret()' -T x | wc -c | tr -d ' ') secret; tip $(h log -r tip -T '{node}')"
