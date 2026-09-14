# A consensus round to voters more than three quarters of a period away expired with every reply still in flight — and since a late pre-vote reply is dropped by design, no council across a WAN could ever elect a leader

- **Date:** 2026-09-14
- **Subsystem:** the configuration council's and root group's rounds on the record plane (`crates/server/src/fleet.rs`: `consensus_budget`, `broadcast`, `drive_council_election`, `late_raft_reply`); the timing law that now derives the budget (`crates/cluster/src/timing.rs::round_budget`).
- **Severity:** fleet correctness on a real WAN — a region whose voters sit more than ~40 ms one way apart (a round trip above 75 ms) could never elect a configuration master, so no membership change, takeover, neighbourhood change or home move could ever commit. Invisible on one host, where every round trip sits inside a heartbeat (measured 2026-09-13: SWIM p99 17 ms, broadcast p99 33 ms).
- **Found by:** the WAN proof for §4.8 "Derived constants" — reading `broadcast`'s stop rule while planning how to inject the fabric's new latency model, then reproducing it by use on the fabric (`crates/cluster/tests/wan_election.rs`, the `FIXED` rule at 80 ms ± 20 ms one way).

## Symptom

At the inter-region profile (80 ms ± 20 ms one way, a 120–200 ms round trip) three voters under the daemon's rule as it stood — the ten-period election timing and the one-period round budget — began 56 campaigns in 300 periods (18, 19 and 19 per node, every 1.0–1.9 s as the jitter rotated) and elected **no leader** in thirty seconds of virtual time (2026-09-14 09:31, seed 11). Every campaign's pre-vote round returned with no reply.

## Root cause

`consensus_budget` gave every round a base deadline of one heartbeat (100 ms) with the lookahead at three quarters of it, and the round's progress witness treats its birth as no progress ("a witness that has never advanced has witnessed no progress, whatever the clock says", `progress.rs`). So at 75 ms, with no reply yet, `DeadlineExtender::evaluate` found the witness not progressing and **expired** the round; `broadcast` returned zero replies, every reply became a straggler. That is correct for a loopback round, whose replies arrive in milliseconds, and it was written for one: "a healthy round completes far inside a period (sub-millisecond loopback RTT plus processing)". On a path whose first reply cannot arrive before 120 ms, every round expired before it.

The rest is by design and correct: `drive_council_election` proceeds to the vote round only on pre-vote replies it received in the round, and `late_raft_reply` drops a late pre-vote reply ("completing a pre-election here would owe follow-on vote requests a settle cannot ship, and the next campaign simply re-runs its pre-vote"). The next campaign re-ran its pre-vote into the same 75 ms stop, forever. A leader, had one existed, would have been fine: its appends still reached the followers (the late replies were folded at the next settle), only their acknowledgements arrived late. The council could hold a leader across a WAN; it could not elect one.

The design already names the remedy: the round's stall window is "derived from the operation's expected per-step latency" (`progress.rs`), and the operation's per-step latency across a WAN is the path's round-trip tail — the same measured tail the election timeout is derived from ("election timeout ≥ 10 × broadcast RTT p99").

## Fix

`timing::round_budget(anchors, tail)`: the base deadline is `max(heartbeat, tail)` and the stall window `max(stall_periods × heartbeat, tail)`, the extension one heartbeat up to `ELECTION_MARGIN` grants, polled `polls_per_period` times a period — the daemon's own anchors, and with no tail measured (or a tail inside a heartbeat) exactly the loopback's budget, so a laptop or LAN fleet runs the budget it ran before (R8). The tail is the slowest voter path's RFC 9002 probe-timeout form of the measured round trips (`PathRtt::tail_ns`), fed by the SWIM probe every period on every node and by every consensus reply, timely or late.

On the fabric, the same harness under the `DERIVED` rule: one campaign per node at 3.17–3.32 s, a leader at 3.62 s, held for the rest of the run, no further campaign; each node's tail 213–243 ms and base 22–25 periods. A leader killed at 15.21 s was replaced at 21.58 s — 6.37 s, inside twice the survivors' base plus span (6.8 s), after one split first attempt. At a geostationary-class profile (500 ms ± 100 ms one way), with the derived budget the fixed ten-period timing still elected (at 15.96 s) and held — Raft's PreVote refuses a lone timed-out follower — but campaigned 59 times against the live leader in 180 s; the derived timing (base 131–133 periods on tails of 1.31–1.33 s) campaigned zero times after electing.

## Impact

- Every fleet whose consensus voters span more than ~37 ms one way (the lookahead point, 75 ms round trip) could not form a configuration master. No deployment has run across regions yet, so nothing in production was affected; the KIND lane and every future multi-region deployment would have been.
- The record plane's commits (`commit_record`) and takeover promotions run under the same budget from `run_record_plane`; they were not blocked (a commit collects acknowledgements from any `f + 1`, and the record re-ships idempotently each period), but every round to far holders was judged uncertain at 75 ms and re-shipped every period until its stragglers were folded. The coordinator now derives its period budget from the slowest measured peer path, so those rounds wait for their first acknowledgement too.

## Exact edits

- `crates/cluster/src/timing.rs`: `round_budget` (new) and the `ElectionTiming`/`PathRtt` law it shares its tail with.
- `crates/cluster/src/lib.rs`: `broadcast` moved here from the daemon so the harness and the daemon prove one fan-out; `request_within` returns the exchange's round trip.
- `crates/server/src/fleet.rs`: the period's budget derived from the measured paths (`consensus_budget` takes the tail), the council and root drives on the shared timer and timing, the path estimate fed from probes and consensus replies.
- `crates/cluster/tests/wan_election.rs`: the reproduction (`FIXED` at the inter-region profile) kept as the non-vacuity contrast, and the derived rule's proofs.

## Verification

Before (the `FIXED` rule, the daemon's own numbers): 0 leaders, 56 campaigns, 300 periods, 09:31 2026-09-14. After (`DERIVED`): 1 leader at 3.62 s, 3 campaigns, 0 after; the LAN histories identical under both rules to the nanosecond; `cargo test -p slates-cluster --test wan_election` 4/4 (0.11–0.93 s wall each on the virtual clock; box load 7.8–11 on 18 cores, which does not bear on a virtual-clock run).

## Siblings

- The daemon's `content_budget` spans its round by `consensus_budget().max_deadline_ns()`; it now takes the period's derived budget, so a content round to far holders is bounded by the same tail.
- `ProbeTiming` already derived the probe's deadline from the measured round trip (2026-09-13); it now reads the shared path estimate instead of a private one, so a consensus reply's round trip informs the next probe's deadline too.
- The handshake's per-flight retransmit ceiling and confirmation wait are RTT-scaled by the transport's own estimator (fixed the same day: `docs/bugs/2026-09-14-transport-rtt-sampled-on-the-wall-clock.md`).
