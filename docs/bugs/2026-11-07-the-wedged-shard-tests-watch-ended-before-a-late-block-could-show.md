# The wedged-shard test's watch ended before a late block could show (2026-11-07, §4.14, A-228)

## Description

`crates/server/tests/liveness.rs` `a_wedged_shard_stops_the_beat` blocks the non-control shard for three windows and
expects the beat to stop, a whole window with no beat, no later than a window and three beats after the block began. It
failed 2 of 5 and then 2 of 10 runs on adacachy, while the daemon met that bound every time.

## Measurement

fixtures, 2026-11-07: main `d61c3be2`'s debug test binary, `--exact --nocapture`, 10 runs one after another in a
`systemd-run --user` unit (CPUWeight=1, Nice=19, CPUQuota=800%, no AllowedCPUs; `nr_throttled` 0 before every run), at
load 126–147 on 32 threads. Runs 2 and 8 failed:

- run 2: the shards initialized in 1.91 s and 1.88 s; the block began 1.394 s after the test's `base`; the beat last
  advanced 2.367 s in (0.973 s after the block began); the watch ended 4.406 s in.
- run 8: the block began 1.052 s in; the beat last advanced 2.047 s in (0.996 s after); the watch ended 4.129 s in.

In both, the daemon logged the shard wedged (1026 ms and 1017 ms with no step and no CPU progress, past the 1000 ms
window) and withheld the beat. Both runs also logged "client identity recovery refused on shard 1" and "the boot is
refused": the block had landed inside the daemon's boot.

## Root cause

Two faults in the test, neither in the daemon:

1. The watch loop ran while `base.elapsed() < held` (three windows from `base`). Under load the blocking observation
   reached the shard 1.05–1.39 s after `base`, so a window-long gap could only be recognized at 3.05–3.37 s, after the
   loop had ended. `stopped_at` stayed `None` and the test panicked.
2. `beating` waited for two beats only. The control loop spawns the heartbeat before it recovers client ids from every
   shard (`control_loop`, `restore_client_ids`), so a test could block a shard the boot was still asking. The beat then
   stopped through the refused boot, a path the test does not name.

## Edits

`crates/server/tests/liveness.rs`:

- The wedged test watches until the block ends (`began + held`), timed from the block's own start, or until its
  observation has answered without the block ever beginning.
- `beating` first waits for a client connection to be accepted, which the control loop does only after the boot's
  client-id recovery, and returns it for the test to hold, so every liveness test starts from a booted daemon.

## Tests

- Red check, macOS arm64 (load 14), 2026-11-07: both tests with the block sent 1.5 s after `base`, as a loaded start
  delayed it (a harness edit, not committed). The old test failed 5 of 5 (the block began 1.62–1.71 s in, the watch
  ended before the gap); the new one passed 5 of 5 (the beat stopped 0.84–1.21 s after the block began).
- adacachy, fixtures' unit shape (CPUWeight=1, Nice=19, CPUQuota=800%, MemoryMax=22G), load 32–51, old and new
  alternating: 10 of 10 each. The shards started in 1–26 ms there, so the late start did not occur.
- macOS arm64, load 14 rising to 80, the new test 10 times: 9 passed; one stopped 1.318 s after the block, past the
  test's fixed bound of a window and three beats (1.3 s). That bound is the next fault, below.

## Siblings (open)

- The wedged test's bound is a fixed slack: a window and three beats. The watchdog paces its looks with a timed wait
  (`recv_timeout(HEARTBEAT_NS)`) and times a wedge from the look that last saw a step or CPU progress. So the last beat
  can come at the block's start plus a window plus up to two of its looks as actually run, and the test sees it a
  sampling interval later. Under load those run late (1.318 s against 1.3 s at load 80 here). The bound should come from
  the run's own looks, the beat word's intervals, not a count of nominal beats.
- `a_spinning_shard_stops_the_beat` bounds the wait by wall time (ten windows). The watchdog judges a spinner by its
  CPU, and correctly calls a starved one alive, so at load 136–153 on macOS the spinner never ran a window of CPU in 10 s
  and the test failed 3 of 3, on main as well as here. The bound should be the spinner's own CPU time (A-65's rule).

- `watch_beat` (the starved, slow-call and spinner-neighbour tests) watches a span from its own start too, but judges the
  largest gap inside work it started itself, so a late start shortens nothing it asserts. Those three passed in the
  suite run at load 80.
