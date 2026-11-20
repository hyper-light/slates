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
- Slope 250->1000 (mh-250/mh-1000 on Studio ~/.cache/w-perf/prof): all linear. M2 corrected: the "Importer" heap is
  svn records' Repository.contents (texts whole), and the green's content pool holds the same files: texts held
  twice. Reported to main + w-svn (slates-work-3d); awaiting main's ruling on M2 (w-svn's code).
- LANDED main 183dd0e23: BENCHMARKS.md "Where the svn server's time and memory go, ranked" (T2 + M1-M5).
- Worktree now ~/slates-work/w-perf-plan (agents/w-perf-plan). Waiting on: w-git push spin, w-hg sliced intake +
  import PastRoom, w-svn green_chains; main's M2 ruling. On each: rebuild on Studio, rerun harness, record.
## A-715 (claimed; main's ruling ~07:00Z): svn texts held once (A-64), after w-svn's charging fix (A-714)
- Readers of Repository::content: records.rs (properties, Contents::mergeinfo, ReadSource::text/properties,
  drive::Green::text), Transaction::content/store/take_fresh_contents (importer delta base + dedupe), export.rs:542,
  svn_commit.rs text_bytes (fresh or records). Engine: Green::content_ref_at(path, version) -> Cow (version N = rev N).
- Seam proposed to w-svn (slates-work-3d) ~07:10Z: contents keep property lists only; texts -> TextPlace{path,
  revision,len}; a lent TextSource (green content_ref_at); log entry's record drops text bytes (green version carries
  them); rebuild replays green then records. Asked: green history trimming? which files A-714 touches?
- Also queued (main): git read path vs git daemon (clone, blob:none, shallow; git/git cut v1.0.0, whole if it fits)
  once w-interop's A-713 clone rows pass (w-interop r22).
- LANDED fdef525e3 (on main, checked): git row at cut v1.0.0 (release 82d3538b0): clone 2.816 vs 0.300 (9.4x slower),
  blob:none 1.092 vs 0.180, push 0.665 vs 0.310; sample: 72% of clone CPU in slates_git::store::deltas::Search::step.
  T4 (reuse deltas) proposed to main; awaiting ruling (w-git's area). A-716 seam awaiting w-svn's reply.
- A-716 seam agreed with w-svn (~07:40Z) with a change: green history IS trimmed (checkpoint_green folds below the
  floor, f37a96515), so texts are keyed by ContentIdentity in a store surviving folding, not (path, revision). The
  record's text bytes leaving the log entry also shrinks apply_advanced's ~245 KB/rev (w-svn hands that to A-716).
  A-714 touches db repository.rs + repository/entry.rs, server repository/push.rs, svn_commit.rs, vcs_store.rs.
## A-718 (main's ruling ~07:35Z; A-717 was w-git's): git deltas computed once and kept; clone streams stored deltas
- Done when: clone --mirror, blob:none, shallow re-measured vs git daemon on the Studio (cut v1.0.0), memory before
  and after, recorded. Ask w-git: can its sliced intake (A-717) record each pushed object's delta as it arrives?
- A-718 built: branch agents/w-perf-plan (pushed as agents/w-perf-a718), land worktree ~/slates-work/w-perf-a718-land
  (agents/w-perf-a718-land, squashed + merged main). Local green before merge: server t_a718+t_a682 6/6, git 2/2, fmt.
  Re-running server test on merged tree (main changed vcs_store.rs, A-714). Studio A/B (cut v1.0.0): clone-again
  2.766 -> 0.912 s (stock 0.342), blob:none 1.076 -> 0.558, shallow 0.154 -> 0.118, first clone 2.791 -> 4.293,
  RSS end 384-388 -> 482-513 MiB. w-git agreed A-718 lands first.
- svn 5,000 import at b705a24c5 (A-714 in): 3,540 revisions then Unpublished (record profile 803 MB > 747 MB segment
  region); svn commit after it E160004 green not at youngest (w-svn bug). Sent main + w-svn.
- QUEUE (main/Ada, ~08:00Z): A-718 land -> A-716 (svn texts once) -> concurrent-push load test (slim protocol client
  replaying a recorded receive-pack, then hg unbundle, svn commit; 1/10/100/1,000/10,000+; per-push memory,
  completions, refusals, p50/p99, footprint; sizes derived; RAM only) -> register entry objects named by identity
  (every tool; seam with w-svn).
