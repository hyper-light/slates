# w-git -> main: items closed (Ada's close list; 2026-11-20 ~18:40 PST)

- **P4, the partial clone regression since A-722 p3: CLOSED.** 7b66e7225 on origin/main ("a walk reads no row for a type
  or for a size it does not use"). Measured: release, cut-1.0.git, ten partial clones each arm, daemon CPU and wall, all
  ten shown in BENCHMARKS.md. Before: wall 0.96-1.05 s, daemon CPU 700-760 ms. After: wall 0.49-0.63 s, daemon CPU 250-360 ms.
  Tests: a_filter_that_reads_no_size_asks_none, a_type_is_read_from_its_landing_with_no_row_read.
- **P5, push_planned's store commit sliced: CLOSED.** ccd07b58c (the pack's holds given back in turns after the landing)
  and 15917f29a (a commit takes what the move's walk found staged). Measured: release, git/git v1.5.0's push, the
  landing step's phases. vcs_store::commit went from 14.0 ms to 0.63 ms. Test:
  a_landed_push_gives_its_holds_back_in_turns_not_in_the_landing.
- **C1, git onto the changeset store: part done, still open.** Record v4 LANDED 7a52ef882 (merge 33f17ba0d; tested at
  c20861789 over origin/main ed021dc42: slates-git 582 passed, slates-server git/vcs_store 166 passed). The pack is homed
  as content and named by identity and length; the record is 141 bytes; the landing step went from 27.3 to 13.2 ms. Its
  shape is A-767's payload section (inbox/w-store-from-w-git-v4-payload-2026-11-20.md). What's left is C5 with w-store:
  the git record and pack land through `slates_store::Store::land`.

## C5, git onto the changeset store: part 1 LANDED (2026-11-21 ~00:45 PST)
- **Commit:** origin/main 0e7120bdd (the merge carrying C5; branch agents/w-git-c5, deleted).
- **Test run:** kiln r13 on the big Studio over ecbeed926 (base origin/main e1f246ca8), with `needs` and
  `timeout_s`. The final merge with main touched none of C5's files (verbs, work_home, work_log only), so no rerun.
- **What it does:** a stock git push (the fixtures' `create`) lands as one changeset on the node's store: `Names` and
  `Payload`, the homed pack's bytes moved in with no further copy, at sequence 1 after the creation's 0, before the
  register's entry. The record, version 5 (it replaces 4), names the sequence. A daemon stopped with nothing more
  published, as kill -9 leaves it, and restarted recovers the repository by reading that changeset back from the
  store: one changeset read, no replay refused, `main` at the pushed commit, its next landing at 2.
- **Red test:** `daemon::tests::t_a767_c5_a_git_push_lands_as_one_changeset_and_a_restart_replays_it_from_the_store`.
  It failed with nothing landed (`landed: Some(0)`) until the landing was asked in the turn before the edge takes it.
- **Results:** green alone (3/3 C5 tests) and in the focused set (211 passed): the C5 tests, t_a727b (now version 5),
  `vcs_store::`, `git_push::`, `repository::store_tests` (w-svn's helper), `store_packs`, `vcs_http::git`, `vcs_ssh::`.
- **Six failures that are not C5's:** `vcs_store::commits::tests::t_a664_live_a_submit_waits_parked_for_its_greens_whole_build`,
  `t_a664_p3_made_commits_name_the_one_before_and_are_made_again_from_the_log`, and four in `vcs_store::paths::tests`
  (`t_a664_p3_every_version...`, `..._a_store_made_again...`, `..._holders_a_successors_rebuilt...`,
  `..._a_branchs_first_version...`). Each refuses the green's submit `Overloaded { shard }`. The three I checked fail the
  same way on origin/main e5baf401c without C5 (kiln r3). Whose are they?
- **The r5 hang:** it was C5's restart test blocked in `StoreOwner::stop` (a store handle outlived both daemons). It did
  not recur after merging 931e52ed6 (A-798's lost wake), alone or in the focused set, so no stack went to w-provision-gate.
  The two samples are kept on the big Studio at ~/.cache/w-git-diag/hang-59097.txt and hang-38760.txt.
- **C5 part 2, next:**
  - git's creation refused on a store-less daemon, naming `--vcs-store`: the git leg of w-hg's
    `a_daemon_without_a_store_refuses_mercurial_and_subversion_naming_the_flag`;
  - versions 2 and sequence 0 removed;
  - git's tests moved onto `test_store`;
  - the replay's pack read a piece at a time through `StorePacks`.

## C5 part 2a: a store-less daemon refuses git's creation, and git's tests are on the store: LANDED (2026-11-21 ~01:05 PST)
- **Commit:** origin/main cf6b3c98a (branch agents/w-git-c5b, deleted).
- **Runs:** the full slates-server lib suite (kiln r22, MacBook, tree 74eef05aa over 0e7120bdd and the later merges):
  1052 passed, 4 failed. Three of the failures are R2's load-only edge tests: `a_long_history_pushed_...`,
  `a_partial_clones_lazy_fetch_...` and `a_push_of_a_long_history_and_many_branches_...`. Two of those fail on main too,
  and `a_long_history_pushed_...` passes alone (r20). The fourth "failure" is just the suite's result line. The four
  `repository::tests` and the A-664 tests are green after 645937436. The format check is clean, and the two changed
  CLI tests compile (2dd14a1af). The final merge touched none of the files I changed.
- **The refusal:** `repository::no_store_for` now refuses git, `Unsupported` naming `--vcs-store`, as it refuses
  Mercurial and Subversion. Red first: w-hg's `a_daemon_without_a_store_refuses_mercurial_and_subversion_naming_the_flag`
  now expects all three refused and none recorded. It failed on git's `GreenCreated` until the refusal landed (r15).
- **Tests moved onto the one helper:** 76 tests across 16 modules move onto `crate::test_store`, using
  `audit_on_stored_shard[_configured]`, `audit_daemon_stored`, `audit_daemon_of_stored` (new) and `create`. The
  HTTP edge suite's daemons are now all `StoredDaemon`s; this also fixed the six Mercurial edge tests that fail on main
  (w-hg told, inbox/w-hg-from-w-git-edge-suite.md). git_interop's and v3_takeover's anchors now pass `--vcs-store`.
- **One test restated:** the rebuild test is now `t_a767_c5_a_rebuild_reads_a_records_names_back_from_the_nodes_store`.
  It reads the changeset back from the node's store; the region-home read is owed a test of its own (in GAPS).
- **C5 part 2b, next:** remove versions 2 and sequence 0, so a homing or landing that falls short refuses the push; and
  read the replay's pack a piece at a time through `StorePacks`.
