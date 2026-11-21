set -e
S=$(cd "$(dirname "$0")" && pwd)
D=~/.cache/w-store-ab; cd $D
export HGPLAIN=1 HGRCPATH= AB_SCRATCH=$D
H=$D/data-hg/hg-src; G=$D/data-git/git-git.git
mkdir -p $D/bin; printf '#!/bin/sh\nexec uvx --quiet --python 3.12 --from mercurial==7.2 hg "$@"\n' > $D/bin/hg; chmod +x $D/bin/hg
export PATH=$D/bin:$PATH
R=$D/data-hg/hg-as-git.git
if [ ! -f $R/AB_DONE ]; then
  git init -q --bare $R
  hg --config extensions.fastexport= -R $H fastexport | python3 -I $S/fi_filter.py | git -C $R fast-import --quiet
  touch $R/AB_DONE
fi
REF=refs/heads/default
echo "hg revs $(hg -R $H log -r tip -T '{rev}'); hg-as-git $REF commits $(git -C $R rev-list --count $REF); git v2.0.0 $(git -C $G rev-list --count v2.0.0) v2.1.0 $(git -C $G rev-list --count v2.1.0)" | tee $D/ab-phase1.log
/usr/bin/time -l uv run --quiet --python 3.12 --with mercurial==7.2 --with zstandard python -I $S/changeset_ab.py $D/ab-phase1.json \
  hg:$H "git:$R@$REF" git:$G@v2.0.0 git:$G@v2.1.0 2>&1 | tee -a $D/ab-phase1.log | tail -25
