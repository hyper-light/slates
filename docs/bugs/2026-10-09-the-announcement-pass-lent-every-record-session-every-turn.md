# The epoch announcement pass lent every record session on every membership turn

**Found:** 2026-10-09, timing every record-session borrow on the two-network bench (`docs/wip/bench/multiregion/mirror.sh`,
10% loss, 150 ± 60 ms each way). **Status: fixed** (same day).

## Description

A diagnostic build timed each borrow of a record session, by borrower (`take_sessions`'s label). It also recorded
which borrower held a session when a mirror content round found none free. In one five-round run, the epoch
announcement pass (`member_task::announce_epochs`) lent a session 23,754 times on a0, 22,577 on a1 and 13,089 on b0.
Every other borrower lent fewer than 1,600 times. On b1 the pass's 1,441 lends held sessions for 494 s in total: 343 ms
each on average, at most 3.1 s. a1's mirror rounds found no free session 123 times.

## Root cause

The membership loop (`member_task::run`) calls `announce_epochs` on every turn, once per received datagram or
heartbeat. The pass borrowed **every** record session through `take_sessions`, whether or not its connection had
already announced, and only then asked `announce_on`, which returned at once for a connection already in
`announced_on`. It walked the borrowed sessions one after the other. An announcement awaiting its answer, for up to the
record exchange budget, therefore kept every other session lent as well. It also kept the membership loop itself from
reading its socket.

## Impact

- CPU and battery: on loopback, the old pass lent about 3,000 sessions a second per node to send two announcements in
  six seconds (the regression test below, run against it: 18,933 and 19,358 lends, 2 and 2 sends).
- Every other borrower (content puts, mirror shipments, forwards, consensus) found sessions out on loan, and a pending
  announcement held all of a node's sessions for up to 3.1 s.
- The membership loop stalled behind a pending announcement.

## Exact edits

- `crates/server/src/member_task.rs` `announce_epochs`: decide the owed sessions inside the state's borrow, from each
  session still in its slot. A session is owed only when its connection id is not the one in `announced_on`. Borrow
  each owed session alone, for its own exchange, and return it before the next.
- Two event counters: `fleet.plane.announce_lends` (sessions lent to the pass) and `fleet.plane.announcements_sent`.
- Test: `crates/server/tests/fleet.rs` `the_announcement_pass_lends_a_session_only_to_announce_on_it`. It forms a
  two-node fleet, lets the membership loop turn for 3 s, and expects lends to equal sends.

## After

- Two five-round runs on the same bench: the pass lent 8–46 sessions per node, against 13,089–23,754 before. a1's
  mirror rounds found no free session 3 and 12 times, against 123.
- Placement time to the mirror was 4.9–8.3 s and 5.4–30.0 s, against 5.5–27.0 s before, at load average 13 on a shared
  machine. No latency change is claimed from these runs.
- The diagnostic's other finding: the remaining misses' holder was a content put of the same seal, waiting on a hedged
  holder's late reply. The next round skips that busy holder, and its put continues; this is benign.
