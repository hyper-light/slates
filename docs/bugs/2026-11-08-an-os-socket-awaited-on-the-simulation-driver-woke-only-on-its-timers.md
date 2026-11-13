# An OS socket awaited on the simulation driver woke only on its timers

**Status:** fixed 2026-11-08 (live-durability). Three failing tests showed it before the fix: two in
`crates/rt/tests/udp.rs`, and the fabric assertion in `crates/cluster/tests/live.rs`.

**Severity:** test evidence only, but every result from the affected tests was suspect. The tests in
`crates/cluster/tests/live.rs` claimed the simulated fabric: its instant paths, its links and its virtual clock. They
ran instead on OS loopback sockets that the simulation driver never watched. Their endpoints were woken only by their
own timers, so whatever they showed about timing or about a link came from probe timeouts. The real runtime and the
transport are not affected: an OS socket on a real driver is watched by kqueue, epoll or the AFD reactor.

## What happened

`UdpSocket::bind` makes a fabric port only on a simulation shard (`on_sim()`). Off any shard, for instance on a test's
own thread before the simulation runs, it makes an OS socket. `crates/cluster/tests/live.rs` bound its sessions'
sockets that way.

A readiness wait names its socket by one handle: the descriptor on a real driver, the port on the simulation's. The
simulation driver's `register_readable` takes any handle for a fabric port. An OS socket's descriptor, say 7, was
therefore registered as interest in fabric port 7, where no datagram ever arrives. Fabric ports are numbered from 1,
so a descriptor could also name another socket's port and be woken by its traffic.

The endpoint's receive tries a non-blocking read before every wait, so the session still made progress. It read the
kernel's queued datagrams each time its own timer fired: the probe timeout, backed off.

## Evidence

- **Every write's round trip.** Over the fabric's instant paths, `a_holder_that_joins_late_is_installed_and_then_follows`
  took 9.0, 14.3, 23.5, 39.7, 68.0, 117.5, 204.2 and 355.8 ms of virtual time for its eight writes, each about 1.7
  times the last. GAPS had recorded this since 2026-11-04 as an unexplained fabric anomaly. Bound on the shard, every
  write takes 0 µs of virtual time.
- **Every wait ended at its timer.** Traced in a stubbed exchange of `a_lost_holder_still_taking_the_request_is_never_cut_off`
  (on `agents/live-durability-watch-2`), each wait ended exactly at the sender's probe timeout: 9, 20, 40, 81, 294,
  762, 1,892, 4,688, 11,607 and 28,728 ms. A burst of acknowledgements already queued then followed at the same
  virtual instant. The fabric's counters stayed at 0 delivered throughout: the traffic never touched it.
- **The link never applied.** That test's 256 kbit/s link was set on the fabric, which carried none of the traffic.
  A lone `request_within` never finished its 60 KB request within 60 s, while a loop re-polling every 100 ms finished
  it in 1.37 s. That is less than the 1.9 s the link would take to serialize it.
- **The transport wakes on arrival.** The same lone request over the fabric, with the socket bound on the shard
  (`crates/transport/tests/exchanges.rs`'s harness), ended waits early on arriving packets. A 100 ms re-poll changed
  nothing there: 47,661 ms either way, the same datagrams delivered.

## The fix

`UdpSocket::readable` and `UdpSocket::writable` now refuse, typed (`RtError::BadConfig`), a wait on a driver that
cannot watch the socket: an OS socket on the simulation driver, or a fabric port on a real one. The next test that
binds off the shard fails at its first wait instead of passing on its timers.

`crates/cluster/tests/live.rs` binds its sockets in a setup task on the shard. Every scenario now asserts that its
datagrams crossed the fabric.

## Sweep

With the refusal in place, every test of `slates-rt`, `slates-transport` and `slates-cluster` was run (cluster with
`--no-fail-fast`, plus its socket-using ignored test), along with the server's lib tests. Only
`crates/cluster/tests/live.rs` was affected: 2 tests on main, and 2 more on `agents/live-durability-watch-2`.

The ignored `wan_election` test fails identically with and without the fix (the same campaign times), for its
documented reason. The three examples that run sessions on the simulation (`fetch_bench`, `class_latency_bench`,
`congestion_bench`) bind inside their spawned tasks.

Both of main's affected tests pass on the fabric. What they assert held. What their comments said about the fabric
did not:
- the round trip that grew about 1.7 times a write is gone;
- "a serve cancelled by a timer every millisecond starved its session" does not hold: a holder serving in 1 ms slices
  passes both tests on the fabric.

The two tests on `agents/live-durability-watch-2`, rebound on the shard, did not both hold:
- `an_exchange_to_a_lost_silent_holder_ends_within_a_silence_not_at_its_deadline` holds. The silent holder is cut at
  1,011 ms; without the watch the exchange runs to its 10 s deadline.
- `a_lost_holder_still_taking_the_request_is_never_cut_off` does not hold. With its 256 kbit/s link now in effect
  (the queue peaked at 77 KB, about 2.4 s at that rate), the live holder's acknowledgements waited behind data. Its
  largest acknowledged packet stayed unmoved for the 1 s silence, and the watch cut it at 1,011 ms. That the watch
  never cuts a holder still taking the request was shown only by the timer-driven run. The silence rule cannot tell
  a dead peer from a second's queueing on the path.

Miri runs CI's `slates-rt` set (the lib, `differential`, `ownership`, `timers`, `readiness`): 60 passed, 8 ignored. The
changed wait is in it: `readiness`'s simulated test awaits a fabric port's `readable`. The two new tests cannot run
under Miri: one needs kqueue, and Miri allows only stream sockets, even with isolation off.

Not covered by this fix: other descriptors awaited on the simulation driver through `readiness::readable` directly
(a pipe, a TCP stream) are still taken for fabric ports. No test does that today.
