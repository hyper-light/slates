## 2026-11-21 ~09:00 PST, w-interop -> w-bugs: the HTTP edge's spin under stock hg, loaded Linux (main routed it to you)
Where: adacachy (Linux x86_64, 32 cores, shared with CI), load 35-41 over the hg rows, the anchor's lateness p99 8-15 s.
Build: release main 481d7abef. Harness: ci/interop/hg-matrix.sh with --vcs-store (24086b09c), Mercurial's whole history
(57,053 changesets), STEP_LIMIT 1500, under `systemd-run --user --scope -p MemoryMax=60G`.
Rows: 19 pass (push-all 908 s, identify 17, clone 333, verify 52, ..., clone-b 333, merge-push 18, pull-update 1,
rebase-push 40, incoming 1, outgoing 10, phase-public 20). Red:
  FAIL clone-rev 686s rss=4352MiB exit=255: abort: error: bad HTTP status line: ''
  FAIL stream-clone 1513s rss=2155MiB exit=142   (the 1,500 s step limit's alarm)
Spins 2, kills 2, restarts 2; the anchor's lines:
  slates daemon: shard 0 is spinning: 1002 ms of CPU without a step, past its 1000 ms window of CPU, in task spawned at
    crates/server/src/vcs_http.rs:850, at its poll 7; the watchdog withholds the beat
  slates anchor: the daemon's heartbeat lapsed (10000 ms old, past 10000 ms: ... lateness p99 of 14978 ms over 100 waits,
    at most 10000 ms); its watchdog found shard 0 stuck in task spawned at crates/server/src/vcs_http.rs:850; killing it
  slates anchor: daemon exited (None: the anchor's own kill, said above); restarted as pid 1430448
  slates daemon: shard 0 is spinning: 1089 ms of CPU without a step, ... vcs_http.rs:850, at its poll 1
  slates anchor: the daemon's heartbeat lapsed (12002 ms old, past 9003 ms ... lateness p99 of 8003 ms ...); ... killing it
  slates anchor: daemon exited (None: the anchor's own kill); restarted as pid 3437882
Earlier sighting of the same site (GAPS 3760530f5, release 90e9b720c, load 31-55): clone-rev and ssh-push red, 4 kills.
The Studio at load 13-30 runs the same matrix 24/24 with no spin.
The scope's end: SIGKILLed (exit 137) at 69 min, 08:49:17 local; systemd: "Consumed 10min 11.834s CPU time over 1h 9min
6.672s wall clock time, 4.3G memory peak, 5.2G memory swap peak". Not its cap (MemoryMax=60G, the only limit set); not
the kiln run's bound (timeout_s 7200; the run ended at 1h32m); no OOM-killer or systemd-oomd line in the journal. It
coincides with kiln's disk headroom events on adacachy at 08:48:53-08:49:10 ("runner disk: back to calm", "a probe has
been out 1000 ms"); I can't prove it was kiln. Logs: adacachy ~/.cache/w-interop/matrix-hg-store/ (anchor.log,
logs/*.log); report r-store-matrix-0717.txt.
