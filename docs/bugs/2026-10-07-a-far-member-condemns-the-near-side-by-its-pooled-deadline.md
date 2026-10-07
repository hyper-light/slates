# A far member condemns the near side by its pooled deadline (open)

**Found:** 2026-10-07, while diagnosing an owner lease that lapsed across two Docker networks
(`2026-10-07-an-owner-lease-lapsed-while-far-members-stretched-the-probe-round.md`). **Status: open.** The defect
is in the vendored failure detector, hyper-swim (`vendor/hyper-raft/hyper-swim`, snapshot `f9a2c8e` of
`../hyper-raft`). The fix belongs upstream and is then re-snapshotted, so it is not changed here.

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
