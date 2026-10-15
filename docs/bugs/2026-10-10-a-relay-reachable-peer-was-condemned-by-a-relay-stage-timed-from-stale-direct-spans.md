# A relay-reachable peer was condemned by a relay stage timed from stale direct spans

**Found:** 2026-10-10, as the relay test's remaining hold failure
(`an_indirect_probe_through_a_relay_keeps_a_peer_the_direct_path_lost_and_losing_both_paths_retires_it`). It failed
2 of 36 and then 3 of 48 concurrent runs at load 60–106. **Status: fixed** the same day, as a local patch to the
vendored hyper-swim, pending upstream.

## Symptom

In the hold phase, B is deaf to A only, so A reaches B only through C. B left A's fleet after 31–34 of the required
100 coordinator periods, although A had 30–38 relay requests answered and credited.

## Evidence

From the failure message, extended with A's detector, its pool and its view of B at the end of the hold:

| A's detector | judged by | expected | margin | mistake bound | suspicions | condemnations |
|---|---|---|---|---|---|---|
| B | own (configured before B went deaf) | 250 µs | 125 µs | 0.713 | 2 | 2 |
| C | provisional | 219 µs | 438 µs | 1.0 | 3 | 0 |

- A's plane: 37 relay requests, 37 relayed answers received.
- C's detector condemned nothing.
- B's own view: alive at incarnation 2, having refuted twice, too late to stay in A's fleet.

## Root cause

- **The relay deadline was borrowed.** The relay stage waited the slowest relay's span plus the target's. Both are
  direct-path verdicts, and B's was frozen at the moment B's direct path was lost: no direct answer renews it.
- **Answers past the period were misses.** The period floor (A-125: 900 ms budget, two members watched) let any
  answer within 128 ms count. At load 100, a relayed round trip crosses three overloaded processes, and some took
  longer than that.
- **Late answers were dropped.** Each one was a miss, so a suspicion. The told probe's relays, in the same burst,
  missed too, so each suspicion became a condemnation of a live member before B's refutation came back through C.
- **The relay stage was never measured.** It is the one probe stream the detector timed without samples of its own.

## Fix

Vendored `hyper-swim/src/detector.rs`, recorded in its `ORIGIN.md`:
- **The timer.** `RelayTimer` is RFC 6298's estimator (`SRTT`, `RTTVAR`, `RTO = SRTT + max(G, 4·RTTVAR)`) over relayed
  round trips, late ones included.
- **Where it applies.** The relay stage waits at least that timeout. A sample taken while relays are awaited lengthens
  the wait.
- **The stated bound.** `period_bound` includes the relay wait.

Slates side: `status` shows `fleet_detector_relays` (the timeout, samples, late answers, the longest), and the relay
test prints it at both phases.

## Tests

- **Failing first.** `a_peer_answering_only_through_a_slow_relay_is_never_condemned`:
  - setup: relayed answers 150–160 ms after each request; refutations 500 ms after each suspicion;
  - before: condemned 491 ms in;
  - after: none in 60 s simulated.
- **Sweep.** 36 runs over latency, spread and seed; see `ORIGIN.md`.
- **Fleet runs.** See the ledger.

## Limits and siblings

- **The cold case.** A relay path slower than a told probe's resolution (about 330 ms under this budget) before any
  relayed round trip has been measured still condemns. Waiting RFC 6298's initial 1 s there would slow a crashed
  member's detection past slates' 900 ms budget. That trade belongs to the budget's owner (§4.8, A-125), not this fix.
- **Unpriced condemnations.** The configurator chose a verdict whose mistake bound is 0.71 for B. Its cost model
  prices a false suspicion as the time to the next probe, not as a likely condemnation. The cost of a false
  condemnation to its owner (a retirement and takeovers in slates) is upstream's design to weigh. It is in the
  handoff.
- **Failures inflate the failure rate.** Every adopted death counts as a failure in the detector's MTBF estimate,
  false and refuted ones included. A false condemnation therefore makes the next verdicts more aggressive. That is in
  the handoff too.

## Upstream (2026-10-10)

hyper-raft took the three handoff items and replaced the local patch (`docs/timing.md` §2.7, "The relay stage" and
"The margin"; `crates/hyper-swim/ORIGIN.md` items 15, 17 and 19):
- **The relay stage.** A relay judges its own leg to the target by its own deadline and nacks when it passes
  unanswered (Lifeguard's nack); each acknowledgement states when a relay's response is due (its longest deadline for
  any member but the asker, plus `G`). The asker's stage ends once every relay asked has nacked, or at its span for
  the slowest relay plus that relay's stated bound. A timer over relayed round trips was rejected: relays are asked
  only when a probe goes unanswered, so on a network that answers every probe it takes no sample and rests at its
  cold second in every bound. A late answer to a told probe takes back its pending condemnation.
- **The cold case** is closed: a relay's deadline is its own, known before any relayed round trip.
- **Unpriced condemnations.** The 0.713 came from the margin being capped at `η − G`, not from the pricing; the
  margin is searched over every margin, and an owner states a condemnation's cost (`set_condemnation_cost`).
- **Failures inflate the failure rate.** A death counts once it stands, once per member and incarnation.

slates' member plane relays as the detector asks: each relayed ping carries its deadline, a nack goes to its asker
when it passes, and a later answer still goes back. `status` shows the relay stage's bound
(`fleet_detector_relays: due_ns stated`) in place of the timer.
