# w-perf (UI goal 93 part "An order of magnitude faster than competing servers, measured and recorded")
Brief: ~/.local/state/slates-fanout/briefs/w-perf.md. Supervisor: SendMessage to "slates-7a".

## Piece 1: A-699 harness + STOCK baselines (git daemon on git/git; then hg serve, svnserve) -- started 2026-11-19
- Predecessor g309's harness: branch agents/g309-scale (a303f3474, crates/cli/bench/git_scale.sh, wip x2, pushed).
  Studio scratch from g309: ~/.cache/g309-scale (src.git = git/git bare at f7de81456940), ~/.cache/g309-target.
- g309's numbers (r526, RUNS=1): stock git daemon push 30.3 s, clone 13.5 s (fsck clean, 1027 refs, 424,851 objects).
- LANDED 2026-11-20 ~04:55Z: main 3691deb5c..9f0851ea0 (piece + merge). Studio r7 (baselines, 04:21-04:47Z), svn tree
  check verified on a 20-commit dump (4,872/4,872). Studio scratch ~/.cache/w-perf: hg-src (Mercurial repo 57035),
  data/git-5000.dump (992 MiB), bench/, clone/ (shallow; read scripts via `git show FETCH_HEAD:...`).
## Piece 2: slates' numbers against those baselines (next)
- Main's rule: own target ~/.cache/w-perf-target (never ~/.cache/slates-local-target); release binary built from the
  named commit; keep >= 20 GB free.
- 2026-11-20 ~04:55Z: release slates built on the Studio from main 9f0851ea0 (A-704 in):
  ~/.cache/w-perf-local-target/release/slates (3m11s, CARGO_BUILD_JOBS=8); its source ~/.cache/w-perf/src-main (depth 1).
- git stock+slates (r13): stock run 1 push 17.8 clone 12.2 (checks identical); slates whole push FAILED: shard 10 spun
  1046 ms without a step, anchor killed the daemon, git "unable to rewind rpc post data" then hung (killed). Evidence
  ~/.cache/w-perf/evidence/. Reported to main; w-git owns the fix (main ~05:05Z), will tell me when it lands.
- Branch agents/w-perf-p2 (worktree ~/slates-work/w-perf-p2), commit "wip: hg_scale.sh measures slates' Mercurial site"
  (SERVERS/SLATES env; start_slates = anchor --vcs-http, repository create --tool hg, token as Basic password via auth.*).
- r19 (hg, SERVERS=slates, RUNS=1, log ~/.cache/w-perf/logs/hg-slates.log, work ~/.cache/w-perf/work-hg2) started
  ~05:00Z; kiln dropped (cert rotation ~05:09Z) before I read it. NEXT: read that log; if it worked, run
  SERVERS="stock slates" RUNS=3 and record; svn's slates side waits on the svn:// listener's repository lookup (A-689).
  Then git once w-git's spin fix lands. Then piece 3 (profile -> ranked plan to main).
