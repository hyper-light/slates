# w-reaper progress (event-driven reaper goal). Supervisor "slates-7a".

## RESUME HERE (2026-11-20, Mac Studio relaunch)
- State branch lives in ~/fanout-state-w-reaper (own folder, main's rule 4d39b11; ~/fanout-state is shared, untouched). An
  A-number is mine only once my push of its amendments.tsv row succeeds (pull, append next free, bump, push at once).
- Piece A (A-709, base hints by readiness + delegation lapse) LANDED: main 9b93fd77c (merged ea05b2ad6). Tests macOS
  arm64, Mac Studio, local: base_hints::tests 1/1 (drained 10.4 ms), deadlines::tests 10/10, slates-base hint_set 2/2,
  bridge-nfs earliest_unreturned 1/1, fmt. Branch agents/w-reaper-hints deleted (landed).
- Studio setup: clone ~/slates-work/w-reaper-hints (its own checkout on local main; land from there), target
  ~/.cache/w-reaper-target (CARGO_PROFILE_TEST_DEBUG=0 to save disk; disk ~16 GB free, under the 20 GB guard from
  other users). After a merge, `touch` the tree's .rs files: mtime fingerprints reuse stale artifacts otherwise.
  Commits need `git -c commit.gpgsign=false` (kiln signer refuses non-kiln sessions); push via
  git@github.com:hyper-light/slates.git (https has no credentials here).
- Pieces B+C (A-715) LANDED: main f70230cc8 (merged 60e23101a). crate::idle_sweep runs the idle sweep at its own
  deadline on the keeper (armed by Store::sweep_owed, arena allocation/allocated-bytes change, deferred frees; re-armed
  while a pass leaves work); daemon::reap_loop deleted -- no reap tick remains. Tests macOS arm64, Mac Studio, local:
  idle_sweep 2/2 (an idle shard ran 0 passes in 3 intervals), give_back 1/1, purge 2/2, deadlines 10/10, base_hints
  1/1, idle 5/5, vfs seal 5/5, fmt.
- Tick-name cleanup (main's last piece, no A-number) LANDED: main 4c6445f (merged ae70481). Docs/names in bridge-core,
  vfs, server, cli; test renamed the_formation_check_allows_every_idle_sweep_event; bench IDLE_TICKS -> IDLE_PAUSE_NS.
- QUEUE DONE: 1 (A-475, on main from g255), 2 (A-522..A-570 g255; A-709, A-715 w-reaper), 3 (A-715 deleted the tick).
- Finding (read, not chased): a start's free-content/recovery give-back (3.2 GB on the Studio) is interrupted by the
  control shard's 100 ms heartbeat every pass, so it takes ~10 passes (one an interval) -- same pace as the old tick.

## State found at start
- On main already (g255): A-475 (dead clients by hang-up), A-522, A-536, A-554, A-570. Queue item 1 DONE on main.
- reap_loop on main ran process_base_hints + delegation::drain and seal_idle_content at LIVENESS_BUDGET_NS.
