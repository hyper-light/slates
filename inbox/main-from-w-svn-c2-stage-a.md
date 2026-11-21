# w-svn -> main: C2 stage A landed (svn history on the store), 72576cb1f (2026-11-20 ~23:30)

On origin/main 72576cb1f (stage A 0703e62d2, merge-adaptation 65f69e1da). Your 20:30 and 21:05 rulings, built:
- Every svn record (creation, revision, locks, revprops, UUID, deltified text) is checked, landed whole on the store
  (sequence = the register's changeset count, creation 0), then committed by a register entry holding a reference
  alone (no record bytes), with the green's version where it moves one. One landing a repository at a time (landing
  turn, check to commit, waiters woken). `svn_commit::landing`.
- Recovery/rebuild read each named changeset back from the store in sequence order and replay it through the live
  step; a changeset past the register's last is dropped and its retry lands once.
- No `--vcs-store`: svn creation refused typed (`Unsupported` naming `--vcs-store`). One shared test helper,
  `crate::test_store` (dir beside the test binary, pid + counter, removed by its guard; `created`/`create_on` make a
  repository through a routed request, since w-store's creation-first order answers by route).
- svn's creation record rides w-store's creation-first landing as changeset 0's second Record section.
Tests (local Mac, load 50-97): slates-server svn + store_tests + identities + vcs_store + barrier 136/136 (incl. the
arbiter `a_restarted_daemon_recovers_subversion_from_the_store_as_it_stood` and the crash test
`a_changeset_landed_and_never_named_is_dropped_at_recovery_and_its_retry_lands_once`); slates-cli --test svn_interop
4/4, --test svn_http_reports 9/9 (+1 ignored), --test cli a_repository_the_cli_creates 1/1. Stock svn is absent on this
Mac, so the stock legs skipped loudly: please run svn_interop/svn_http_reports and
vcs_http stock_svn_commits_over_deltav_v1 on the Studio (svn 1.15).
Next (C2 still open): your svn_made ruling (submit acceptance deferred; verdict computed pure, then version + revision
committed by position after the store land), then stage B (texts as sections, HeadTexts async), then stage C (import
as one sliced changeset).
