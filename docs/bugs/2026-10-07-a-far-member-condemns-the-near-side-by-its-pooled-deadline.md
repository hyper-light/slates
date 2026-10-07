# A far member condemns the near side by its pooled deadline

**Found:** 2026-10-07, while diagnosing an owner lease that lapsed across two Docker networks
(`2026-10-07-an-owner-lease-lapsed-while-far-members-stretched-the-probe-round.md`). **Status: fixed in
slates' vendored copy (2026-10-07); owed upstream as hyper-raft branch `swim-pair-deadline`.** The defect is in the
failure detector, hyper-swim (`vendor/hyper-raft/hyper-swim`, snapshot `f9a2c8e` of `../hyper-raft`).

## Description

Two regions on two Docker networks, joined by a router shaping 100 ms ± 40 ms one way and 3 % loss each way. Every
daemon's log showed members falsely dead and refuting, including members of its own region:

- in its first minute, `a0`'s log folded 33 refutations of `b0`, 28 of `b1`, 30 of `b2`, 9 of `a1` and 6 of `a2`;
- `a1`'s status, after about 30 s, showed 99 suspicions of `a2`.

Each such death is a membership change: gossip, a refutation, a session's address resolved afresh.

## Reproduction (simulated, no loss)

`slates-cluster` `tests/member_plane.rs`, `no_live_member_is_condemned_across_a_lossless_far_link` (ignored while
open; run with `-- --ignored`):

- members 1 and 2 are near each other;
- members 3–6 are near each other and 100 ms one way from 1 and 2;
- the network is lossless, with a 100 µs jitter.

In 30 s, every far member condemned both near members, 76 to 115 times per pair. A trace of the detectors' own
findings (temporary, 2026-10-07) showed:

- every crossing probe's answer was due 1.9–2.7 ms after its send, on a path whose round trip is 200 ms;
- the condemnations ran at 20–49 per pair per 10 s, from the start to the end of the run, so the pair never
  recovered;
- the near members never condemned a far one.

## Root cause (by the detector's own words; to be confirmed upstream)

hyper-swim judges a pair by its own estimator once that estimator configures. Before then it judges by the member's
**pooled** estimator, "every round trip the member measured to anyone" (`detector.rs` module doc). A far member's
pool is mostly its three same-side peers' 1 ms round trips, so the pool's verdict puts every crossing probe's
deadline near 2 ms. The pair's own estimator never takes over: every crossing probe is judged unanswered at its
deadline, and the pair never gathers the evidence it needs to configure.

The near members' pools are mostly far round trips, so their deadlines are generous. That is why the condemnations
run one way.

## Impact

Any fleet whose round trips are not all alike falsely condemns live members without end:

- two regions, or one region across availability zones;
- a fleet where some peers share a host and others do not.

Every death costs gossip, a refutation and a session re-resolution. A death also feeds every decision taken on
membership. Measured here:

- region 1's location rounds for a region-0 volume answered `Unavailable`;
- before the lease renewal, the owner's lease cohort lost a holder from the detector's view.

## What a fix must hold (for the upstream change)

- A pair's probes must not be judged by a deadline that the pair's own round trips have already exceeded. A pooled
  verdict is a fallback for a pair with no evidence, not for a pair whose answers arrive late every time.
- A late answer is evidence of the path, not of a loss. The pair should configure from it.
- The test above must pass, and the existing suites must hold: a killed member is still condemned by every survivor
  within the stated bound.

## Cause, confirmed (2026-10-07)

The detector's own counters showed it. Each far member's estimator for a near pair had taken zero round trips in
30 s, although that pair's answers kept arriving (its `last_answer_ns` was set). The member's pool read a 1.1 ms
round trip with 45 % loss, its "losses" being those very probes. A peer keeps only its last three probes'
records (`OUTSTANDING`), and the far member probed each peer about every 10 ms, so every 200 ms answer found its
record already reused: no sample, no configuration, and the pool's 2 ms deadline judged the pair for ever.

## Fix (`vendor/hyper-raft/hyper-swim/src/detector.rs`)

- **When a pair stops being judged by the pool.** A pair is marked as not fitting the pool (`Peer::pool_misfit`)
  when its keying handshake measured a round trip longer than the pool's deadline, when an answer comes back past
  that deadline, or when an answer arrives after its record was reused. Such a pair is not judged by the pool: its
  probes are measurement only until its own estimator configures.
- **Its own backoff.** A misfit pair's measurement wait backs off per pair (`misfit_misses`, doubling to the
  existing cap; RFC 6298 (5.5)). The member-wide backoff is reset by every near answer and would never reach the
  far path. The pair's answers therefore become its samples.

## Tests

- `no_live_member_is_condemned_across_a_lossless_far_link`, no longer ignored. Before: 76–115 condemnations per far
  pair. With the late-answer rule alone: 1 per pair. With the per-pair backoff and the handshake round trip: 0,
  3 of 3, every far pair configured at the true 201 ms.
- `a_far_member_that_dies_is_condemned_by_every_survivor_and_no_live_one_is`: a killed far member is held dead by
  every survivor, at least one by its own probes within the stated bound, and no live member is condemned. 3 of 3.
- hyper-swim's own suite passes 74/74; the plane suite passes 8/8; the fleet suite passes 73/73.

## Follow-up: the handshake read a stale pool verdict (2026-10-07)

**Found:** the far-link test failed once in a cluster suite run. It runs on virtual time with deterministic jitter;
the member plane's own randomness (probe order) varies the run. Over 40 runs it failed 5 times, each with one
condemnation of near member 2 by one far member (`(3, 2)` three times, `(4, 2)` once, `(6, 2)` once). The "0, 3 of
3" above was too few runs to see it.

**Cause, from a trace of each miss (temporary, 2026-10-07):** both condemning misses were judged by the pooled
verdict (a 2.5 ms span), with the pair not marked misfit although its handshake had measured 201 ms. `start`
compared the handshake with `self.pool.verdict` and only then called `pooled()`, which configures the pool when it
is due. On the probe that first configured the pool, the comparison read no verdict, so it found no misfit, and that
probe was judged by the freshly configured 2.5 ms verdict.

**Fix:** `start` takes the pooled verdict first (`pooled()`, configuring it if due), then compares the handshake
with it, and judges by it only when the pair fits.

**Tests:** `no_live_member_is_condemned_across_a_lossless_far_link` passes 100 of 100 runs (35 of 40 before);
`a_far_member_that_dies_is_condemned_by_every_survivor_and_no_live_one_is` 30 of 30; the plane suite 8/8 three
times; hyper-swim's suite 74/74.

## Left open

On the lossless link, the far pairs' estimators report 15–40 % loss: measurement periods that ended before their
answer, counted as losses. It condemned nothing, but it widens their margins. The upstream change should decide
whether a late answer within the pair's wait is a loss at all.

**From the upstream review (hyper-raft owner, 2026-10-07): a liveness gap, open in the vendored copy too.** A misfit
pair's probes are measurement only until its own estimator configures, and an unanswered measurement probe
condemns nothing. A member that dies before its far pairs configure is therefore never condemned by those pairs.
`a_far_member_that_dies_…` passes only because the dead member's near peers condemn it and the condemnation spreads.
With members 1–2 near and 3 far and 3 killed early, no survivor would condemn it.

The fix owed: a judged deadline for a misfit pair from the evidence it has (the pool's measured shape shifted by the
pair's measured round trip), derived in hyper-raft `docs/timing.md` beside §2.7, with the test "a far member killed
early, every survivor far, condemned by every survivor within the stated bound".
