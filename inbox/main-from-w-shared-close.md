# w-shared: closed items (goal 93's shared pieces), base origin/main ed77b1d30

Each item below is on origin/main (`git merge-base --is-ancestor <commit> origin/main` holds for each).

1. **Inputs held at B** (A-697 (1): B holds its own composed inputs, so A's record to B is record-only).
   Commit f26544b65 (landed by fb692bdd5). Test: `cargo test -p slates-server --test fleet
   a_holders_composed_inputs_are_never_shipped_back_to_it`. It asserts `merge.inputs.held_by_composer` >= 1 and
   that no input chunks are shipped to the composer (`merge.inputs.chunks_shipped` stays 0 toward B).
2. **Sleep-and-look waits, family one** (a lent record session's return wakes its borrower).
   Commit c03a8953b (landed by 86e23c4ef). Test: `cargo test -p slates-server --test fleet
   a_forward_waiting_for_its_lent_session_is_woken_by_its_return`. It asserts `fleet.session_wait.returned` >= 1.
3. **Sleep-and-look waits, family two** (a dispatch's stragglers are woken by their reports).
   Commit 54b9d8b2b (landed by ab30bfb75). Test: `cargo test -p slates-cluster --test live
   a_lane_that_ends_with_a_holder_unanswered_gets_every_session_back`. It asserts woken >= 1 and ticked == 0.

Still open, in close-list order:
- Completion: A-697 f, then A-658 3c.
- Robustness: family three (the FUSE helper's exit is woken by a pidfd or NOTE_EXIT watch; agents/w-shared-sweep-3
  2aa0d1782 is waiting for its Linux run, r6), then exit 4 at 64 MiB.
- Performance: the lane's ~200 MB/s, then the content-plane-versus-lane A/B.

The sleep-and-look item closes when family three lands.

## Closed 2026-11-20 ~18:05 PST: A-697 part f (takeover + routing flip; the hook deleted in the same change)
Commit a9e789e54 on origin/main (landed as 56f33c6f6). `Daemon::serve_works_on_holders` / `WorkHome::on_holders` are
gone. The rule "replicated here" (`work_home::green_elsewhere`) is in force: the control shard reads `merge.replicas`,
and every other shard reads the fanned `WorkHome::holding`. The list is fanned per period and at once on a replica's
first version.
Tests (macOS arm64, local; on the merged tree):
- `cargo test -p slates-server --test fleet a_holders_work_is_committed_by_its_greens_successor_after_the_owner_dies`
  (new, 3 nodes at f = 1; the survivor that doesn't take over submits after A dies; answered v2; counters
  `merge.work.computed_on_holder` and `merge.verdict.taken_by_position` at the successor moved).
- The two-node e2e on production routing:
  `a_holders_work_is_committed_by_the_verdict_its_replica_computed_taken_by_position_at_the_owner`.
- Filters: fleet `a_holder` + `a_works_route_ends` 26 of 26; lib `work_home` + `fleet::tests` 23 of 23 (new
  `a_replica_taken_reaches_the_other_shards_with_its_owner_once`); fmt clean.
- Red, with the rule off: both holder tests failed ("the work's id names B", "the work was made on the holder").
A-697 is complete with this (parts a1-f). Please mark it met.
