# w-reaper progress (event-driven reaper goal). Supervisor "slates-7a".

## RESUME HERE (2026-11-20, Mac Studio relaunch)
- Piece A (A-709, base hints by readiness + delegation lapse) LANDED: main 9b93fd77c (merged ea05b2ad6). Tests macOS
  arm64, Mac Studio, local: base_hints::tests 1/1 (drained 10.4 ms), deadlines::tests 10/10, slates-base hint_set 2/2,
  bridge-nfs earliest_unreturned 1/1, fmt. Branch agents/w-reaper-hints deleted (landed).
- Studio setup: clone ~/slates-work/w-reaper-hints (its own checkout on local main; land from there), target
  ~/.cache/w-reaper-target (CARGO_PROFILE_TEST_DEBUG=0 to save disk; disk ~16 GB free, under the 20 GB guard from
  other users). After a merge, `touch` the tree's .rs files: mtime fingerprints reuse stale artifacts otherwise.
  Commits need `git -c commit.gpgsign=false` (kiln signer refuses non-kiln sessions); push via
  git@github.com:hyper-light/slates.git (https has no credentials here).
- NEXT: piece B: idle seal + purge by aging deadline on the keeper (seal_idle_content/purge_if_idle in daemon.rs),
  then piece C: delete reap_loop + its tick, test that no reap waits on it, §4.7 says so, LOOP_TASKS -1.

## State found at start
- On main already (g255): A-475 (dead clients by hang-up), A-522, A-536, A-554, A-570. Queue item 1 DONE on main.
- reap_loop on main ran process_base_hints + delegation::drain and seal_idle_content at LIVENESS_BUDGET_NS.
