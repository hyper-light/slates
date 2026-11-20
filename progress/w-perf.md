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
- ~05:30Z hg by use: whole push 413; chunked 2000 and 250 also 413; import refused PastRoom{room:278528}. Reported
  to main (w-hg owns). A-710 (`slates repository import NAME PATH`) LANDED main 5c6eb940d with hg_scale.sh's slates
  side (SERVERS, SLATES, PUSH_CHUNK, SLATES_FILL=import). Test: Studio `cargo test -p slates-cli --bin slates
  repository` 3/3, fmt clean. Studio release binary now at 543606da3 (~/.cache/w-perf-local-target).
- NEXT: svn slates side (A-695 lookup + CRAM sign-in landed): svn_scale.sh SERVERS/SLATES, fill by
  `slates repository import project git-5000.dump` (svn), then svn:// checkout/log/commit/update vs svnserve.
- ~06:20Z LANDED main a11712269: svn_scale.sh slates side + KEEP_LOGS + svn rows in BENCHMARKS.md (1,000 revisions,
  main bc2803d66: import 1.874 vs svnadmin load 62.817; log 0.062 vs 0.140; checkout/commit/update even) and the
  blockers list. Reruns sent: svn 5000 at 9ea6e1311 still capped at 1,685 (w-svn = slates-work-3d); hg A-711 at
  bc2803d66: 413 gone, shard 10 spins 1,152 ms, daemon killed (w-hg = slates-work-09). Studio binary = bc2803d66.
- WAITING on: w-git push-spin fix, w-hg unbundle slicing + import PastRoom, w-svn green_chains cap. On each landing:
  rebuild release on the Studio (~/.cache/w-perf/src-main, target ~/.cache/w-perf-local-target), rerun that tool's
  harness SERVERS="stock slates" RUNS=3 KEEP_LOGS=..., record rows.
- Meanwhile piece 3 (profile slowest working paths -> ranked plan to main): svn checkout/commit (client-bound?),
  import memory (2.5 GB for 1,000 revisions).
## Piece 3: profile -> ranked plan (started ~06:30Z 2026-11-20), Studio, release bc2803d66
- svn checkout (1,000 revs): wall ~4.2 s both; client user 0.48 sys 2.78 (client-bound); server CPU slates 0.12 s vs
  svnserve 0.56 s. log: server CPU ~0 both.
- Footprint after import: 250 revs (85 MiB dump) ~920 MB; 500 (144 MiB) ~2,060 MB; 1,000 (223 MiB) ~2,430 MB
  (MALLOC_LARGE/SMALL ~2/3, VM_ALLOCATE (content pool) the rest). `status --json` has no memory counters.
- malloc_history (MallocStackLogging=1 on the anchor; daemon inherits), 250 revs, live heap 577 MiB:
  Importer::import 118.7; Repositories::apply_advanced 86.5 (259 allocs, ~334 KB/revision); merge history
  Value::whole 51.9; Partition::apply_green_checkpoint 46.8 (2 allocs); Green::commit 43.9; UnitSet::whole 42.7
  (segment, fixed); apply_repository 39.7 (257); merge engine apply 31.5+20.5; thread stacks 47 (fixed).
  Virtual only: timer Wheel::new 898 MiB (39 MiB a shard), segment/content-pool maps.
- Plan sent to main ~06:45Z; awaiting its pick.
