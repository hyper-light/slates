# A daemon marked itself serving before its shards' starts ended (2026-11-24, A-311, A-228)

Found by main 01a20182's r865 on the big Studio (svn use round, git-2000's dump, 77397a5f4): once the import's own step
spun past its window and the anchor killed the daemon, every restart's shard 0 spun past its window in its start
(`start_shard`, polls 12-13) and was killed again: 1,189 restarts in 44 minutes, shard 0 initialized once and shard 1
1,190 times. The anchor logged each "daemon pid N recovered: it has served as long as its start took (102 ms), so its
exit is no failed start ... (0 failed starts in a row)".

## Description

The anchor's restart bound (A-311, `slates_anchor::supervise`) counts a daemon's exit before it has served as long as
its start took as a failed start, a second in a row a crash loop backed off from the failed start's own life. Its start
ends at the daemon's serving word (`layout::SUP_SERVING`), which the daemon writes "once its start, recovery included,
is done". The daemon's role wrote it as soon as `Daemon::start` returned, but `Daemon::start` returns once every shard's
start is seated, not ended: each shard's start, its recovery included, runs alone on its shard in slices (A-228). So
the word said "serving" about 100 ms in, before any shard's recovery ended; a start that never ended was a recovered
daemon's exit every time, restarted at once, never counted.

## Impact

Any daemon whose shard start could not finish (a recovery step past the watchdog's window, as r854's and r865's were,
docs/bugs/2026-11-24-a-replays-choice-of-named-runs-was-cubic-in-a-pushs-files.md) restarted at once, forever, at a
start's cost each time, where the bound would have backed it off. `slates status`'s `recovered` (the anchor's verdict,
`SUP_RECOVERED`) said recovered of a daemon still recovering, so a test or an operator that waited for it waited on the
wrong event.

## Edits

- The control loop marks the daemon serving (`daemon::mark_serving`) after its boot (`boot_before_claims`, which asks
  every shard and is answered by each only once its start ended; the loop itself runs on the control shard, which runs
  nothing until its own start ends) and, on Windows, the anchor's extent holds.
- The role (`slates_api::role::daemon`) no longer marks it.
- Red: `recovery::a_daemon_serves_only_once_every_shards_start_has_ended` (shard 1's start held busy for two liveness
  windows: the word unmarked while it starts, then marked no sooner than the held start; before, never marked by an
  in-process daemon, and by the role at once).

## Sweep

Every writer of `SUP_SERVING`: the role (removed) and the control loop (now). Every reader: the anchor's supervisor
(`supervise.rs`, the start's end and the recovered verdict) and `slates status`. The anchor's own failed-start count is
unchanged and held by its tests, which mark serving themselves.

## The end-to-end test (main's order of 06:05)

The bug lived between the daemon's word and the anchor's count, and each side's own tests passed. So the test that
holds the two together runs across processes on `common::supervised`:
`recovery::a_shard_start_that_never_ends_is_a_failed_start_and_its_loop_backs_off`. A supervised daemon's second
shard's start spins in one poll past its window on every start (`BootFaultKind::Spinning`), and the test plays an
anchor that kills a daemon whose beat lapsed (`supervised::watch_restarts`). Expect: the first exit a failed start
restarted at once, the second a `CrashLoop`, its restart no sooner than its backoff, no exit read as recovered. Green
here: a failed start at 19.3 s, a crash loop at 30.5 s backing off 11.1 s, the restart at 41.7 s (debug, the MacBook,
44 s in all). Red with the bug put back (the serving word marked as `Daemon::start` returned): three exits, each
`Restarted { failed_in_a_row: 0 }`, restarted at once, never a crash loop, as r865's 1,189 were.

