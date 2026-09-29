# Correlated election jitter livelocked a split vote

Date: 2026-09-28. Scope: the configuration groups' election timer (`crates/cluster/src/timing.rs`,
`ElectionTiming::timeout_periods`), which every council and root-group election in the daemon uses. Found by
the pre-vote audit on the timed simulation (`crates/cluster/tests/prevote.rs`,
`docs/wip/research/consensus-enhancements.md` §3.1).

## Symptom

On the multi-region profile (80 ms ± 20 ms one way), a three-voter group whose leader was cut off took 3.0–3.4 s
to elect a successor in most of twenty seeds, but 12.0 s in one and 19.3 s and 19.9 s in two. Seed 0's campaign
log shows the two survivors timing out 23 ms apart, again and again:

```
campaigns after the cut (ms, node): (2571, 3), (2594, 2), (5171, 3), (5194, 2), (7871, 3), (7894, 2),
                                    (10671, 3), (10694, 2), (13571, 3), (13594, 2), ...
```

The group recovered only when the partition healed (the cut node returned); with a permanently dead leader it
would never have elected.

## Root cause

A follower's timeout is `base + jitter` periods, the jitter a deterministic stand-in for Raft's randomized
timeout (§5.2, §9.3). It was `(local + attempt) mod span`, with `attempt` advancing at each campaign — meant to
rotate two colliding nodes apart ("break their tie within a few attempts"). But when two survivors both
campaign each round, both attempts advance together, and a shared increment preserves `(local_a + attempt_a) −
(local_b + attempt_b)`: two nodes congruent mod the span stay congruent forever. Each round they time out
together; each grants the other's pre-vote (both have lost their leader); both start a real election for the
same term and vote for themselves; the vote splits; both wait the same timeout again.

Member ids are 64-bit hashes, so a given pair of survivors starts congruent with probability about `1/span`
(one in ten at the floor) — after any restart that resets both timers, for instance.

## Fix

The draw is `splitmix64(local ^ attempt · γ) mod span` (γ the golden-ratio increment; Steele, Lea & Flood,
OOPSLA 2014): still deterministic, so a simulation reproduces from its seed, but independent across nodes and
across attempts. A collision now repeats with probability about `1/span` per round.

## Tests (each fails on the old draw, passes on the new)

- `prevote::survivors_of_a_leader_loss_elect_within_a_few_election_timeouts` (by use, timed simulation, both
  profiles, twenty seeds): a successor within four election timeouts. Old draw: multi-region seed 0 took
  19,114 ms against a 12,800 ms bound. New: 3.0–3.6 s, one seed 8.3 s (one collision round); cluster 1.4–1.6 s.
- `timing::tests::two_nodes_draws_stay_independent_across_shared_attempts` (property): over 4,960 pairs and
  offsets of 64 shared attempts, the worst pair collides fewer than 24 times (a fair draw's maximum reaches 24
  with probability 2.2 × 10⁻⁵; measured 17) and the mean is about one in ten. Old draw: 64 of 64.
- The two timer unit tests that pinned the old formula's outputs ("host 3 waits 10 + 3"; "the rotated attempt
  makes the next timeout one period longer, so two colliding followers drift apart") now assert the rule
  (fires at `timeout_periods(local, attempt)`, contact resets the age) — the drift claim was the bug.

## Impact

Any group of voters could livelock after a leader loss whenever two survivors' draws were congruent, which a
fresh or restarted pair is with probability about one in ten. **Possibly** the unexplained CI failure of
`a_warm_fleet_restart_recovers_its_root_and_regional_quorums` (run 36498699200): a warm restart resets every
timer, and a council election between two collided survivors would never finish inside the test's 30 s. Not
confirmed — that failure's assertion now prints each survivor's consensus state, which would show both
campaigning in lockstep with climbing terms.

## Sibling sweep

- The SWIM detector's timings (`detector.rs`) seed their jitter from a golden-ratio mix of the node id already.
- No other timer in `crates/cluster` or `crates/server` derives jitter from `id + counter`.
