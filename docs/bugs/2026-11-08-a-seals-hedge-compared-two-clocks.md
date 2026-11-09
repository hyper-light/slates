# A seal's hedge compared two clocks, so it never waited for its measured delay

**Found** 2026-11-08, mutation-testing the rewritten hedge test (class B of the starved-machine timing tests). A mutant
that made the hedge delay thirty seconds left `a_slow_first_round_candidate_is_hedged_after_the_measured_p95` passing.

**Root cause.** `content_work` (`crates/server/src/fleet.rs`) holds a seal's hedge round back until its first round has
been outstanding for the measured p95 put latency (§4.8: "hedged to the remaining candidates after the measured p95 put
latency"). It compared `state.clock.monotonic_ns()`, the machine's boot clock, with `first_round_at_ns`, which
`put_seal_content` stamps from `futures::now_ns()`, the shard driver's clock. The two have different origins, so the
difference was the machine's uptime: measured 45,791 s against a delay of 30 s. Every seal hedged at its next look,
whatever the p95. The stamp came in with the clock-keyed hedge of 2026-09-13
(`docs/bugs/2026-09-13-hedge-keyed-on-placed-round-count-never-widens.md`, 35a0ac1b), so this held from then.

**Why no test saw it.** The hedge test holds the first-round candidate and asks that the seal place through the hedge.
A hedge that fires at once also places through the hedge, so the test passed either way. Only a delay longer than the
test's hold could tell them apart, and only a mutant made one.

**Fix.** `content_work` reads the clock the stamp was taken on (`futures::now_ns()`). A new counter,
`fleet.seal.hedged`, counts each hedge round actually sent.

**Evidence.**
- Unit test `the_hedge_waits_out_its_delay_on_the_clock_its_first_round_was_stamped_on`, on a real shard: a first
  round stamped now with a measured p95 of ten seconds hands out no round; at a p95 of one nanosecond, past it, the
  round is a hedge.
- The hedge mutant (a thirty-second delay) is now red: the owner counted no hedge before the hold's ceiling
  (CI diag-clients-mutants-3 and -4).
- Every server target on the fix, Linux x86_64 and arm64 (diag-clients-timing-all-1, -2, -4): nothing relied on the
  immediate hedge.

**Impact.** Since 2026-09-13 every seal sent its content to all remaining candidates as soon as its first round was
looked at again, not after the measured p95: one more copy per seal than the design's rule, on every placed seal.
Durability was never short (the extra copy is redundant and reclaimed); network and holder load were higher than
designed, and any seal benchmark since then includes it. A seal that places within its delay now sends no third copy.
