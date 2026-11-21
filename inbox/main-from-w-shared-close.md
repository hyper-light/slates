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
