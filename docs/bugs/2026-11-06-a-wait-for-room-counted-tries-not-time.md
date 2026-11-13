# A wait for room counted tries, not time (2026-11-06, §4.9, §4.14, the busy machine)

## Description

Two of the server's waits for room in a full control channel bounded themselves by a count of tries, each followed by
one pause of `REPLY_PACE_NS` (10 ms):
- `xshard::given_room_within`, a scatter part's wait (half the liveness budget, so `budget / pace + 1` = 51 tries);
- `xshard::given_room`, `send_back`'s wait for a result carried home (`REPLY_ROOM_ATTEMPTS` = 11 tries, "one
  coordinator period").

A pause on a busy shard comes back only after the poll under way ends, so each try lasted a pause plus a busy poll,
and the wait stretched with the shard's load: not a time bound. The pauses' results were also discarded
(`let _ = sleep(..).await`).

## Measurement

macOS arm64 (M5 Max), 2026-11-06, each worktree on its own target dir, the starver a task spinning 20 ms a poll and
yielding:
- `a_status_on_a_starved_shard_is_refused_overloaded_on_time` (`crates/server/tests/ack_release.rs`): on main the
  status answered in **1.96 s**, past the 1 s hold, so the part was sent once the hold ended and the scatter was not
  even refused. The test that led here, `a_daemon_status_whose_part_cannot_be_sent_is_refused_overloaded`, had failed
  once at load 62–68 on its timing bound.
- `a_result_sent_home_to_a_full_channel_waits_its_period_in_time_on_a_starved_shard` (`crates/server/src/xshard.rs`):
  on main the wait gave up after **986 ms** against its 100 ms period.

## Root cause

A budget expressed as a number of tries of a fixed pause holds only while each pause takes its nominal time. It doesn't
on a busy shard, which on a busy machine is the normal case.

## Edits

- `crates/server/src/xshard.rs`: `given_room_within` waits until a deadline on the shard's clock (`budget_ns` from
  its start), each pause the lesser of the pace and what is left; a refused pause (only off a shard) is counted
  (`xshard.room_pause_refused`) and ends the wait. `given_room` is `given_room_within` with `REPLY_ROOM_NS`, one
  coordinator period (`HEARTBEAT_NS`, derived; `REPLY_ROOM_ATTEMPTS` is gone).
- `crates/server/tests/ack_release.rs`: the status case beside a busy shard. And `check_typed_refusal`'s upper time
  bound is the client's reply deadline, not the hold. The typed `Overloaded` reply proves the refusal was decided inside
  the hold; its delivery from a busy shard is not the design's to bound by the hold.

## Tests, after the fix (same host and dirs, load 33–52)

- The status beside a busy shard: answered 542–724 ms, refused `Overloaded` (3 of 3; main 1.96 s, not refused).
- `send_back`'s wait: 102–127 ms over 5 runs, against bounds of 164–209 ms. The bound is the period, plus twice the
  starver's longest poll as measured in wall time (one run's 20 ms spin was preempted for ~200 ms), plus one pause
  (main: 986 ms).

## Sweep

Every other counted wait in non-test code is a time bound already, or no wait:
- the Windows hold's `acquire` (enclosed by its exchange's `within`);
- `observe.rs`'s retries (paused up to the deadline);
- the client's transient retries and reconnects (elapsed against the reconnect budget);
- `fill_task_arena` (bounded by the arena, a fill);
- the DNS resolver's attempts (`resolv.conf(5)`'s own semantics);
- `receive.rs`'s `READ_ATTEMPTS` (test-only, spins without pausing).
