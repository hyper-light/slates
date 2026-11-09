# A starved origin gave up its forwards to a live owner (2026-11-09, A-232 class C+D; §4.8 "Lookup", §4.14)

Found by clients' false-death runs (A-232), named in the supervisor's ruling, and fixed here on the forward lane's
owner's side.

## Description

A forward batch waits on its owner through the record session's forward lane (`forward_lane::forward_batch`). It is
given up once none of its forwards has made progress for its stall budget: a liveness budget plus the path's measured
round-trip tail. That quiet was the wall clock on the origin's control shard: `now − last progress`.

A starved origin's control shard runs in slivers. Its forwarders wait for their turn on the session, the driver reads
the replies late, and the wall runs on meanwhile. So the time the origin's own thread could not run was counted as the
owner's silence, and batches to a live owner were given up. The client was then refused or left waiting, even though
its peers knew the owner.

## Measured

- Clients' runs: an origin at nice 19 beside two spinners a CPU was refused its read after 10 to 12 s, with
  `fleet.forward.batch_given_up` at 20 to 25 against 2 batches begun.
- `crates/server/tests/fleet.rs` `a_starved_origin_keeps_its_forward_to_a_live_owner`, on adacachy2 (32 CPUs): b's
  control shard at nice 19 pinned beside two spinners at nice 0 on its CPU (`common::starve::Depth::Nice`, about 0.7%
  of the CPU), 32 clients reading through b, a full ring each:
  - main: failed 6 of 6, b giving up 2, 713, 30, 122, 826 and 1,208 batches to the live a;
  - the fix, per forwarder (each forwarder waking a pace at a time): passed 4 of 4, none given up;
  - the fix as landed: passed 3 of 3, none given up (load 41 to 47).
- The same fix on the dead-session test (256 clients, a deaf session): the first answer on the replacement 83, 105 and
  147 ms after the drop on adacachy2 and 47 and 152 ms on this Mac, no late beat.
- A silent owner's forward is still given up at its budget: `a_silent_forward_gives_up_while_its_session_carries_others`,
  2 of 2.
- Reads served are b's CPU's to say, not the lane's: 1,024 reads in flight outran a shard at 0.7% of a CPU on both arms
  (4 to 11 served in 11 s, the rest unanswered, b's own refusal at its failover SLO included).
- Two things the test needed, found on the way:
  - One serial reader did not reproduce it. With b's control shard starved, main served every read (18 to 29, the
    longest 2.3 s) and gave up nothing. The give-ups need forwards waiting for the lane's driver, as in clients' runs.
  - SCHED_IDLE beside 63 spinners (`Depth::Idle`, A-228's extreme) held b's shard off its CPU for seconds at a time, so
    nothing on it ran: no reply in 11 s on either arm, and no batch reached its budget.

## Fix

The quiet is counted on the lane's poll clock (`forward_lane::Quiet`). Each lane keeps one clock. The driver's turns
last at most one poll pace (`HEARTBEAT_NS / POLL_PER_PERIOD`, 10 ms), and each turn credits the clock its wall time up
to the larger of a pace and the CPU time the shard ran meanwhile (`slates_rt::registry::shard_cpu`). Each forwarder
marks the clock at its start and at its batch's progress, and its quiet is the clock's advance since then.
- An idle shard wakes on time and is credited about the wall, so a dead owner's batch is still given up at its stall
  budget.
- A busy shard wakes late but ran the whole time, and is credited the wall too.
- A starved shard wakes late having run little, and is credited a pace a turn, so its own lateness is not taken for
  the owner's silence.
- The wall still bounds it: a batch quiet for the failover SLO (`DaemonConfig::failover_slo_ns`) is given up however
  little its shard ran.
- Waiting forwarders sleep until woken, or until their polls left would run out on the wall, and then look at the clock
  again.

Two versions came first, and the measurements say why neither stands:
- Each forwarder credited its own waits, a pace at a time. It held the starved test (4 of 4, none given up), but woke
  every waiting forwarder a hundred times a second: thousands of forwarders a lane at the rung's load.
- One clock a lane, credited a pace a turn whatever the shard ran. It held the starved test too (3 of 3 once the
  spinners were at nice 0), but it could not tell a busy shard from a starved one. With a deaf session under a
  256-client burst, batches to the deaf owner outlived their budget, piled up, and kept the control shard's beat 0.5 to
  6.5 s late. The first answer on the replacement came 1.06 to 2.21 s after the drop on adacachy2 against 43 to 136 ms
  on main's lane (alternating A/B, load 52 to 81), and 22.4 s on this Mac against 26 to 336 ms.

A driving turn now returns the session after at most one pace when nothing arrives. That makes the dead-session
judgement's return cadence a pace at most for a lane that is waiting, not a whole stall budget.

## Why not the shard's CPU clock alone

`observe::ShardTime`, which judges a cross-shard call in its target's own time, cannot judge this wait by itself. An
idle shard runs little CPU, as a starved one does, so a dead owner seen from an idle origin would be waited for until
the failover SLO, not the stall budget, and failover would slow ten times. ShardTime tells them apart by the shard's run
state, but the forwarder runs on the shard it would judge, which then always reads as running. Here the CPU clock only
separates a busy shard from a starved one; the poll pace separates an idle one.
