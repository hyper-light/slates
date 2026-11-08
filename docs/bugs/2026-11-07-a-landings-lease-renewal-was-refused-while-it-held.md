# A landing's lease renewal was refused while its lease held

**Found** 2026-11-07 by the `landing_fairness.rs` flake on main (fixtures: 2 of 6 on adacachy at load 30+;
`a_landing_longer_than_its_lease_term_renews_it_and_lands_everything` ended `partial` where `done` was due). The
failing run reproduced on an Apple M5 Max (18 cores, macOS 26.4.1) at load 11 to 215 with no spinners. Under two
`yes` spinners a logical CPU, the lease test failed 1 of 3 and the patient test 1 of 3 (6 of 6 passed quiet).

**What the logs showed.** Each entry the landing skipped `LeaseEnded` was fenced against the lease the landing first
took (generation 7, expiring one term after the landing began), though the daemon counted renewals. Every renewal
`keep_lease_alive` made inside its margin was answered `LandingLeaseHeld { holder: 1 }`, naming the landing's own
attempt. It was not kept, and the lease stayed at generation 7. In one run (term 3,239,360,000 ns, longest gap
between two looks at the lease 502 ms, inside its 1.62 s margin), 29 of 500 entries were skipped this way.

**Root cause.** The keepalive (AUD-29-25, 2026-10-01) renews by taking the target lease again as its own holder. The
partition's guard (`Partition::check_landing_lease`, `crates/db/src/partition.rs`) refused any take while the target's
lease was unexpired, its own holder's included. That rule came from AUD-29-03 (2026-09-29), written when nothing
renewed. It was meant to stop two landings of one principal, which then shared a session as their holder, from
taking each other's lease; the same change made the holder one landing attempt. So a running landing could renew
only once its lease had lapsed. Every entry it reached between the lapse and its next look at the lease was skipped.
On a quiet machine that window is microseconds and no entry falls in it. Under load it holds a slice or more.

**Impact.** Every granted landing longer than its term (the failover bound, 10 s by default) lost the entries it
reached in each window between a lapse and the next renewal. It ended `partial` and needed a second landing to
finish. Nothing was written wrongly: a skipped entry is left for a later landing, and the overlay keeps it.

**Fix.** The guard refuses a live lease's take by another attempt only. The holder's own take renews it, under the
newer generation the guard already required. Failing test first: `landing::tests::
a_holder_renews_its_own_live_lease_and_no_other_holder_can` was refused `LandingLeaseHeld { holder: 20 }` before the
fix. AUD-29-03's test no longer expects the attempt's own second take to be refused.

**The test's own bound.** `landing_fairness.rs` ran its keepalive test at a fixed 1 s term. Its 500 ms renewal margin
is shorter than what a busy Mac's shard goes between two looks at a lease (up to 1,205 ms measured). A run like that
tests a lapse, not the renewal. The daemon now records that gap (`LandingState::longest_lease_gap_ns`, in
`SliceSummary`). The test derives its term from the gap its calibration landings measured, four times the gap and
never under a second, and makes a run again whose own gap reached its margin.

**Siblings checked.**
- The entry fence (`Landing::fence`, `crates/land/src/engine.rs`) reads its clock as `request.now_ns` (taken when
  the landing began) plus the time since the write phase began, so it runs late by however long the phases before
  writing took. That errs toward writing past a lapse, the unsafe side. Reported, not changed here.
- A client's plain call answers `Stalled` at the fixed 1 s reply deadline (`LIVENESS_BUDGET_NS`) on a live daemon
  (seen here as a `create` during shard 0's 3.4 s start at load 215). That is liveness's entry in GAPS.
- `begin_landing` plans every entry in one poll (489 ms for 1,000 files at load 150), against the bounded-work rule.
- No tries-count wait (`xshard::given_room_within`'s class) is on this path. The client's ring-full send is
  time-bounded, and the one-shard tests never scatter.
