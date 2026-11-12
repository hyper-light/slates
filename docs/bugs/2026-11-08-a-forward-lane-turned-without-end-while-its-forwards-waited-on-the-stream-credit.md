# A forward lane turned without end while its forwards waited on the stream credit (2026-11-08, §4.8 "Lookup", §4.3)

Found on the f = 1 primary-kill rung (1,000 clients, adacachy) by loadtest-scale, on A-257 (liveness's 577fe8e1)
merged with A-252 and the load-test share: in every run, 6 of 6, the client node's control shard logged "shard 0 is
spinning: 1,07x to 1,09x ms of CPU without a step", the watchdog withheld its beat, and its anchor killed and restarted
client-0. The rung's oracle was unaffected; each run paid a client-node restart.

## Description

A forwarder that drives a peer's lane takes turns: borrow the session, settle the lane (begin queued forwards, take
replies), drive the session, settle again, return it. `drive_or_queued` ended a turn at once whenever the lane's queue
was not empty, so that a newly queued forward is begun without waiting for the open ones' silence.

After the primary restarts, about 20,000 forwards wait for it on the client node, and the session's stream credit
admits a few at a time. The queue is then never empty: every turn begins nothing (`Backlogged`), its drive returns at
once, and the forwarder turns again in the same poll. The task never returned `Pending`, so the shard never stepped,
until the forwarder's stall budget ran out.

## Measured

- Before (6 runs): spinning 6 of 6 (1,070 to 1,098 ms), heartbeat lapse 6 of 6, two restarts a run (the primary, then
  client-0), oracle 0.
- Where: `perf record --call-graph fp` on client-0 during the spin, a release build with frame pointers and line
  tables. Shard-0 had 75% of the process's samples, and 67% of them were under one task poll:
  `ShardContext::step → poll_slot → verbs::forward_to_owner → resolve_and_forward → fleet::forward_batch_over_leader_session
  → forward_lane::drive_or_queued` (28.8%), with `forward_lane::settle` (12%) and the endpoint's drive and flush.
- After (6 runs, e6ad6ba2 on the same build, load 22 to 40): spinning 0 of 6, no lapse, one restart a run (the primary,
  which the restarted node's boot line names), oracle 0, 14.5 to 17.7 s.
- `crates/server/tests/fleet.rs`, `a_silent_owner_leaves_no_forward_the_lane_admitted_unrun`, which puts about 20,700
  forwards in flight through b's control shard to an owner held silent, now asserts that b's control shard never ran
  one poll as long as the watchdog's window (`ShardPulse::longest_step_ns`, `LIVENESS_BUDGET_NS`).

## Fix

- A turn ends early only for a forward queued since the lane's last settle (`Lane::fresh_queued`, set by `enqueue`,
  cleared by `begin_queued`). Forwards still waiting on the stream credit do not end it: the drive waits for a packet
  or a timer.
- A forwarder yields to the shard after every turn it drives: a drive returns `Ready` whenever a packet is already
  there, so under a burst of replies the turns never wait either.

## Also fixed here, measured but not the cause

The first fix (3dfc8a1b) took the profile's hot leaves for the loop: its samples were unwound from DWARF and had no
callers. With it alone the rung still spun 6 of 6. What it changes stands on its own:
- A turn asked every open forward's reply (`take_reply`) and progress (`reply_bytes_read`), each a full drain of the
  session with a clock read. It now takes only the completed exchanges (`Endpoint::take_completed`) and marks only those
  that read bytes (`Endpoint::take_progressed`, new), through an exchange-to-ticket index; it wakes only the forwarders
  it answered; a replaced session is swept once. A drain with nothing readable reads no clock.
- A forwarder leaving scanned the whole queue (`retain`): quadratic in a burst. The queue is an ordered set.
- `Endpoint::completed` was emptied only by `take_completed`, which a record session never calls, so it grew with every
  exchange such a session completed (banned item 8). `take_reply` and `abandon` now take an exchange out of both sets,
  and the endpoint census counts them, so every session's leak witness in `crates/transport/tests/exchanges.rs` holds
  that they keep only open exchanges: with `take_reply` not clearing them, four of those tests report the client leaked.

## Not changed

`slates_cluster`'s `take_completed_replies` (`lib.rs`) asks each of a dispatch's exchanges with `take_reply`. It runs
on the same record sessions as the forward lane, and the endpoint's completed set has one consumer, so it cannot take
that set without taking the lane's completions too. With a drain that reads no clock when nothing is readable, its
cost is a lookup a holder: bounded by the dispatch's holders, not by the forwards in flight.
