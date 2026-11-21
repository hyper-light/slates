# main <- w-balance: closes (Ada's order, 2026-11-20 16:20)

## C1 CLOSED: goal 93, "record v3 at f+1 with the takeover test"
- The item: a git push's record at version 3 (its names by BLAKE3 plus count, homed in the region at f+1 before the
  push is answered), switched on, and proven by a takeover test.
- Commits on origin/main: 08fba4007 (`vcs_store::write_version_3` true in non-test builds) and e73283fd6 (the takeover
  test asks the successor with the owner's token; the grant-all workaround gone, A-764's proof). Checked today:
  `git merge-base --is-ancestor <c> origin/main` true for both. Built on f95edf68e (push records ride the names log,
  a push answered only once held at f+1) and 3406df928 (the region half on the control shard).
- The test: `crates/cli/tests/v3_takeover.rs` (gated by SLATES_TEST_CLI): three `slates anchor --fleet` at f = 1,
  stock git over HTTPS, the owner SIGKILLed with a push in flight; asserts NoLoss, TotalOrder and StaleNeverCommits
  (fsck of the whole clone in order). It was red on v2 records (the successor served the repository with no refs).
  Green 4/4 with v3 on (08fba4007), then 3/3 with the owner's token (e73283fd6). One run's counts: the successor
  replayed 3 records and read one record's names from a region home (`names_fetched` 1); each survivor homed 2
  objects and served 19 region exchanges.
- The measurement: BENCHMARKS.md "Three nodes: a version-3 push's homing at f + 1", v2 (f95edf68e) against v3, ten
  rounds all shown: 11 of 11 `homed_in_region` every round after 3406df928; one push v2 1.67 to 5.28 s, v3 2.04 to
  3.83 s; ten at once p50 v2 4.31 to 11.36 s, v3 5.65 to 8.73 s (Studio-2, load 10 to 16, noisy). Still owed
  (recorded in GAPS): where v3's added time goes at ten pushes at once, on a quiet host.
