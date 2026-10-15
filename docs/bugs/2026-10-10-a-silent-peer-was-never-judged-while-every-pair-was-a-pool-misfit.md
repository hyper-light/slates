# A silent peer was never judged while every pair was a pool misfit

**Found:** 2026-10-10, as the remaining intermittent in the relay test's first phase
(`an_indirect_probe_through_a_relay_keeps_a_peer_the_direct_path_lost_and_losing_both_paths_retires_it`). It failed
3 of 60 concurrent runs at load 60–117. **Status: fixed** the same day, as a local patch to the vendored hyper-swim,
pending upstream.

## Symptom

- In phase 1, B is deaf to A, and the test waits for A to probe B through the relay C.
- In the failing runs, A never asked a relay: `relays_asked` stayed 0 for 4,000 coordinator periods.
- B refused 4,488 of A's datagrams, so A was probing, yet A's detector suspected B 0 times.

## Evidence

The test message gained each pair's misfit flag and estimator refusal, and the member's pool (hyper-swim
`PoolReport`, slates `DetectorPoolReport`, in `status` too). The caught run, A's detector:

| | configured | judged by | samples | pool misfit | refused |
|---|---|---|---|---|---|
| B | no | measuring (no verdict) | 141 | yes | `CorrelationUnmeasured` |
| C | yes | own | 140 | yes | — |
| pool | no | — | 5 | — | `CorrelationUnmeasured` |

## Root cause

- **Both pairs were misfits.** Under load, an early answer from each peer came back after three later probes had
  reused its record. Each pair was marked a pool misfit, permanently, until its own estimator configured.
- **The pool starved.** It is fed only by the pairs it fits. It took 5 round trips and never configured.
- **B had no verdict.** Its own estimator refused at 141 samples (correlation unmeasured under load). A misfit pair's
  provisional verdict needed the pool's, so it had none.
- **B was never suspected.** Once B stopped answering it gave no more samples. Its probes stayed measurement only,
  so it was never judged.

The detector's design held probes as measurement only "while the pool refuses too", expecting the pool to configure
within a few dozen round trips. Nothing guaranteed that it would.

## Fix

Vendored `hyper-swim/src/detector.rs`, recorded in its `ORIGIN.md`:
- While the pool has no verdict, a pair without its own verdict is judged provisionally once it has been silent,
  from its earliest probe still unanswered, for the whole of that verdict's deadline.
- The deadline is RFC 6298's timeout: `3R` from the pair's latest round trip, rounded up to the RFC's 1 s minimum
  (§2.4). Without the pool, nothing has measured the host's stalls.
- Before that, probes stay measurement only, so a pair that answers is sampled as before.

Slates side:
- `status` reports each peer's `pool_misfit` and `refused`, and a `fleet_detector_pool` line.
- The relay test prints both at each phase.

## Tests

- **Failing first.** `a_silent_peer_is_condemned_when_every_pair_is_misfit_and_the_pool_never_configured` (vendored
  detector):
  - before: the silent peer is alive after 600 s simulated;
  - after: condemned.
- **Restated.** `nothing_is_judged_before_the_estimates_exist` now asserts the 1 s boundary. Its old 40-period form
  held only because the pool had not yet configured.
- **Counts.** 81 of 81 detector tests and 25 of 25 cluster test binaries pass.
- **Fleet runs.** See the ledger.

## Siblings

- **First contact.** A pair with no round trip at all, while the pool has none, is now judged at 1 s of silence as
  well. That is the same RFC 6298 initial timeout the pool-backed first-contact rule uses. For example, in a two-member
  view whose only other member is silent from the start, its pool is never fed.
- **Sticky misfit flag.** The flag is never cleared, even when later round trips fit. A near pair marked by one stall
  stops feeding the pool for good. That is upstream's design to weigh; it is noted in the handoff.
- **Still open: the hold failures.** In 2 of 36 runs, A retired B with every relay answered. That is a separate
  mechanism, open in GAPS.
