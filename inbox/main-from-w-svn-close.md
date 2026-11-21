# main <- w-svn: close notes (Ada's CLOSE THE OPEN ITEMS)

## C1 DeltaV v1 -- CLOSED
- Commits on origin/main:
  - 84c74cc52: part 1, reads (vcc, bln, ver; the update-report at the VCC).
  - 2bf76bbbb: parts 2+3, commit verbs over DavCommit at the daemon's HTTP edge.
  - 3e63bee8d: part 4, by use.
- Tests:
  - stock_svn_commits_over_deltav_v1_on_a_daemons_loopback_edge: stock svn 1.15.0's mkdir, checkout, add, commit and
    log against a repository whose OPTIONS withholds the v2 headers, as SVNAdvertiseV2Protocol Off does. At least two
    v1 activities made. Passed on the big Studio, kiln r15.
  - a_deltav_v1_commit_lands_as_mod_dav_svn_lands_it: ra_serf 1.14's seven requests through the edge.
  - dav::activity tests, and site::tests deltav_v1_*.
- Departures stated in GAPS: a short MERGE answer, a path-only Location, the out-of-date check at the next request.
- Next: C2, svn onto the changeset store with w-store.

## R3 the hook test flake -- CLOSED
- Commit: 74beb5600 on origin/main.
- Cause: the hook tests waited ten liveness windows, and their reader returns part-way at a timeout. A loaded
  machine's first hook run outlasted the wait, so the test saw a cut answer as a wrong one.
- Fix: the wait is a hang's bound (HOOK_PATIENCE_NS), and the unlock test checks its answer is whole first.
- Evidence: 8 of 8 runs of an_unlock_runs_pre_unlock_and_post_unlock and
  a_commit_over_svn_runs_its_repositorys_commit_hooks green (load 11-17). The vcs_svn, vcs_hooks and svn_hooks suites
  green.
- Please remove the run-requests.md entry.
- Red reproduced, as you asked at 15:35. A post-unlock hook that sleeps past the old ten-window wait (12 s against
  10 s) gave:
  - with the old wait: an_unlock_runs_pre_unlock_and_post_unlock failed exactly as the flake did, the answer cut at
    "( success ( 7:new.txt ) ) done (";
  - with HOOK_PATIENCE_NS: passed.
  This was a one-off local run, not committed.

## R5 the exporter's rebuild sliced + the red svnserve-over-SSH test -- CLOSED
- Exporter: A-762, 507d722b3 + 2cf1b1d35. TextSource::read_step reads a delta chain one window a step, a stored text
  one store window a step. Test an_export_rebuilds_a_long_delta_chain_a_window_a_call: no call reads a text whole, at
  most one step a call, red on the old exporter. Measured: 6.18 ms longest call against a 6.04 ms whole read before.
- SSH red: b1206d96c. vcs_ssh::svn::tests::the_daemons_ssh_site_serves_svnserve_t_and_lands_a_stock_commit was red
  5/5 and is green after: a landing that put no text waits on no durability barrier.

## R4 recovery replays a revision record whole: sliced -- CLOSED
- Commit: 25035f50c on origin/main.
- The change: a Subversion revision record's replay (recovery and rebuild, record_turn -> svn_record_turn) applies
  its rows once and keeps its texts one row a step, across turns, until the shard's quantum runs (RevisionReplay;
  counter vcs_store.svn_replay_turns).
- Tests:
  - a_replayed_revision_keeps_its_texts_a_row_a_step_as_a_whole_commit_does (svn): the same records as a whole
    commit, byte for byte.
  - a_revision_record_is_replayed_a_turn_a_file_at_a_rebuild (server): twelve 50 KiB files over at least twelve turns,
    with the same records and green.
- Left in GAPS: one row's text is still deltified whole within its step.
- C2 status: waiting on w-store's answer about how the server opens a repository's Store (inbox
  w-store-from-w-svn-stored-form-2026-11-20.md). Next: P6 svn:// update 5.6 s vs 2.7 s.

## C2 svn onto the changeset store -- IN PROGRESS
- Part 1 LANDED 00ff3b25c. A client's svn commit (land_work: svn://, svn+ssh, DAV) lands its revision as one
  changeset through the shard's StoreHandle: sequence = revision, judged over the one before, Record section = the
  revision's record. The client is answered once it is durable.
- Test: a_subversion_commit_lands_as_a_changeset_in_the_store. Red first on the real store: the log recovered from its
  files holds the creation and revision 1.
- Replay from the store waits on w-store for a section read through the owner, and on your ruling with w-store on
  whether recovery replays from the store or the register (inbox w-store-from-w-svn-replay-2026-11-20.md).
- Next while waiting: green-made versions' changesets. Then an import as one changeset (its sequence rule asked).

## P6 CLOSED (2026-11-21 ~01:20): svn:// update 5.6 s vs 2.7 s
- Commit on origin/main: e69375553 (in 40c3f8d1e): `measure_svnsync_replay_range_over_the_tunnel` now also times an
  update across `SLATES_SVN_UPDATE` (FROM:TO) and a checkout, as stock svn asks them, through the daemon's tunnel site.
- Measurement (BENCHMARKS "Subversion on the changeset store", release, MacBook Pro 2, load 20-30, git-2000):
  update r1000..r1500 0.14 s (2.3 MB, the edit whole), checkout r1500 0.46 s (24.6 MB). w-bugs' run 13 (base
  2d3f218cb, before w-perf's per-reader WindowCache b51f908f2) was 5.6 s for a stock client's whole update, its longest
  step a window unit; the server's drive is now 0.14 s of that. Re-running w-bugs' diag harness end to end on main is
  yours if you want the stock-client wall.
- My earlier "not whole" 524 bytes was my script's: the client's closing success sent during the drive (E210004, as
  svnserve answers it).

## P8 CLOSED (2026-11-21 ~00:50 PST): the 500 MB commit, re-measured
- Commit on origin/main: 002a7944f (BENCHMARKS "A 500 MB Subversion commit, re-measured on the changeset store").
- Measurement: big Studio, stock svn 1.15, main 73cd23c (C2 stage A + B1), `a_large_commit_measured` release. kiln r9
  (load 5-9): svn:// Committed revision 1 in 11.2 s, DAV in 13.9 s; daemon peak 4,067 MiB (8x the text; 12x before);
  longest shard step 1.91 s. kiln r8 (load 8-14): DAV's daemon restarted within that step, E160007.
- Left, outside svn and recorded in GAPS/BENCHMARKS: the green's apply of the landing in one step (w-engine's 1b),
  and the text carried twice in the register's log (chain entry and checkpoint, 524 MB each; P7's finding).
