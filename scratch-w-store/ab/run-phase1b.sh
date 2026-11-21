set -e
# A-767 item 7, phase 1b: serving a same-tool clone and an old version's read, bracketed with stock tools.
# git: (a) pack-objects reusing stored deltas and zlib streams = serving the landed pack as received;
#      (b) pack-objects recomputing every object and delta = serving from normalized content with no cached pack.
# Best of 3, all 3 shown, wall and max RSS (macOS /usr/bin/time -l).
D=~/.cache/w-store-ab; cd $D
G=~/.cache/w-store-ab/data-git/git-git.git
t() { /usr/bin/time -l "$@" 2>$D/t.err >/dev/null; awk '/real/{r=$1} /maximum resident/{m=$1} END{printf "%ss %dMiB\n", r, m/1048576}' $D/t.err; }
for rev in v2.0.0; do
  for i in 1 2 3; do echo "git a reuse $rev #$i: $(echo $rev | t git -C $G pack-objects --revs --stdout -q)"; done
  for i in 1 2 3; do echo "git b recompute $rev #$i: $(echo $rev | t git -C $G pack-objects --revs --stdout -q --no-reuse-object --no-reuse-delta)"; done
done
# old version reads: a file at the first, middle and last tenth of v2.0.0, cold-ish (one process each)
N=$(git -C $G rev-list --count v2.0.0)
for frac in 1 5 9; do
  c=$(git -C $G rev-list --reverse v2.0.0 | sed -n "$((N*frac/10))p")
  for i in 1 2 3; do echo "git read tree+all blobs @${frac}/10 #$i: $(t sh -c "git -C $G ls-tree -r $c | awk '{print \$3}' | git -C $G cat-file --batch >/dev/null")"; done
done
export HGPLAIN=1 HGRCPATH=
H=~/.cache/w-store-ab/data-hg/hg-src; export PATH=$D/bin:$PATH
for i in 1 2 3; do echo "hg bundle (revlog deltas) #$i: $(t hg -R $H bundle -q -a -t none-v2 $D/b.hg)"; done
NH=$(hg -R $H log -r tip -T '{rev}')
for frac in 1 5 9; do
  r=$((NH*frac/10))
  for i in 1 2 3; do echo "hg read all files @${frac}/10 #$i: $(t hg -R $H archive -q -r $r -t files $D/arch-$frac-$i)"; done
done
rm -f $D/b.hg
