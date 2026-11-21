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

## R2, the load-only edge tests: CLOSED (2026-11-21 ~01:00 PST)
- **Commit:** origin/main 970950081 (fix d9c014fa0).
- **Cause, found with a timing diagnostic (kiln r28, main cf6b3c98a, six concurrent copies of the three tests on the
  MacBook at load 24):** 18 of 18 failed, each with one reply gap of 6.7 to 7.0 s, and no long step anywhere else. A
  push's answer under side-band-64k wrote its first keepalive two intervals after its body: `body_end` waited one
  interval and wrote nothing, then the reply waited its own interval before appending it. That made 10 s, the client's
  patience, so any push answer past 5 s failed.
- **Fix:** the keepalive is written as soon as it's due, the silence measured from the last write. The homing and the
  store's landing run on a task the exchange awaits across its keepalives, and the report is held until the push's
  records are held at f + 1, with the keepalive written meanwhile.
- **Red, then green:** the same six-way run (kiln r29, d9c014fa0, load 29 to 33): 18 of 18 passed, against 18 of 18
  failed before. `vcs_http::`, `git_push::` and `vcs_ssh::`: 168 passed. Format clean.
- **Owed (GAPS):** the SSH edge's receive still carries its homing and landing inline, with no keepalive between.

## C5 part 2b, piece 1: a client's push writes version 5 only: LANDED (2026-11-21 ~01:10 PST)
- **Commit:** origin/main 3ce898533 (branch agents/w-git-c5b1, deleted).
- **What changed:**
  - The push's homing is always on; the test switch `write_version_4[_for_test]` is gone.
  - Every shortfall refuses the push `PushFailed::Unlanded` (`GitRefused::Unlanded` at the append), for the client's
    retry: a carried homing that falls short, a pack not held, no store, a refused landing, or names that changed
    between the homing and the append.
  - An append that brings no object writes version 5 naming none.
- **Red, then green:** `t_a767_c5b_a_push_whose_homing_falls_short_is_refused_and_records_nothing`. On main 970950081
  it was red: answered `unpack ok` / `ok refs/heads/main` (r30). On the branch it is green (r32: 288 passed, format
  clean). On the merged tree (426f6466c, which touched vcs_store.rs): 288 passed, 1 failed. The failure is
  `git_push::tests::t_a752_a_push_over_a_flat_head_builds_its_work_a_unit_at_a_time` (`past_budget` 1). It is
  load-sensitive: it passes 3 of 3 alone at load 22, and failed the same way under load before this piece (r18).
  Please add it to the loaded-machine list.
- **The one register-held path left, version 2, at `vcs_store::SealedPush::record`'s last arm (`record_of`):**
  - server-made commits (A-664), until they take w-svn's deferred-submit landing order (piece 3);
  - pushes whose driver never takes its homing ask: the tests' synchronous drivers `git_push::tests::answered` and
    `pushed` (piece 2).

## C5 part 2b, piece 2: a homing ask no edge takes refuses the push; and a hold lost at a home's restart: LANDED (2026-11-21 ~01:55 PST)
- **Commit:** origin/main f5a14f6a8 (branch agents/w-git-c5b2, deleted). Version 2 now remains only where nothing homes
  the record: server-made commits (piece 3) and a store test that seals a push straight into the store.
- **The untaken ask:** an ask no edge took by the turn after now refuses the push `Unlanded`, counted
  `HOMING_UNTAKEN`.
  - Red, then green: `t_a767_c5b_a_push_whose_homing_no_edge_takes_is_refused`. Red on main 3ce898533 (r36: the push
    answered, `None`); green on the branch.
  - The tests' synchronous drivers carry the ask through a stand-in edge (`git_push::tests::carry_blocking`): the
    home's own `keep_for_region`, and a real landing from a thread of its own. Their async drivers use the edge's
    `carry`.
- **Two real bugs the suite found on the way:**
  1. **The HTTP edge returned for a keepalive before taking a homing ask** made in the same step, which leaves it
     untaken. That is harmless before this piece and would have refused pushes after it. The ask is now taken first.
     The test was `a_long_pushs_answer_is_kept_alive...`, which closed the connection with `UnexpectedEof`.
  2. **A replay never took back its record's hold on a homed pack or names when this node is their region home.** The
     region's hold, restored first, made the content look held. Every restart of a home left that content one hold
     short. The replay now counts the region's hold apart, in both the whole and the stepped replay. Red was
     `t_a727_repository_windows_survive_a_warm_restart_in_the_shard_image`: 5 holders before the restart, 4 after.
- **Runs:**
  - full slates-server lib on the branch (r45): 1060 passed, 1 failed. The failure is
    `content_ranges::tests::a_status_requests_sweep_...`, which also fails on main.
  - merged tree f5a14f6a8 (r53): focused 290 passed, format clean.
- **Now:** the priority push cut-off. A release build is done, and the repro samples the daemon's git counters during a
  stock git push. One finding already: w-interop's incremental push (one commit) is cut at 56-60 s too, so it isn't
  the pack's size.
