# The hedge test reconstructed a different candidate order

Date: 2026-09-20. Design: §4.8, §4.10, AC-8.12.

## Evidence and impact

The Linux io_uring workspace run failed the hedge history with placement after
4.486584211 s against its 3 s whole-control-shard hold. This timing failure is still
under investigation; the following test defect is independently confirmed.

Logging the real content dispatch showed that the test sometimes held a different
node from production's first content candidate. One passing history selected
9822634609003556681 while the first put targeted 10148797249270668608. Such a pass
does not prove hedging a slow first candidate.

A new observation of the owner's actual placement made the discrepancy fail before
the hold: expected first holder 5623983685493326116, actual 13129814755544900089.
Command: `cargo test --offline -p slates-server --test fleet
 a_slow_first_round_candidate_is_hedged_after_the_measured_p95 -- --nocapture`
in the approved disposable Linux container with `SLATES_TEST_DRIVER=io_uring`.
The failing history took 2.94 s; log `/private/tmp/slates-hedge-order-red.log`.

## Cause and fix

`remote_candidate_indexes` reconstructed placement using the control shard's
neighbourhood and an empty failure-domain map. Production uses the object's owner
shard's committed configuration, including domains; domains participate in copyset
ordering. A neighbourhood alone cannot reconstruct that order.

Expose `Daemon::placement_candidates(object)` through the existing typed observation
seam. Select the fault target from that owner-shard result. Preserve the timing and
content-placement assertions; do not lengthen the hold or accept a later placement.

## Validation

With actual candidate selection the timing failure reproduced immediately at
3.332623669 s. Its separate cause was queued replies discarded at expiration;
see [the collector diagnosis](2026-09-20-collectors-expire-before-reading-queued-replies.md).
After that correction, ten serial Linux io_uring histories passed (281.630–506.180 ms),
with the original three-second assertion unchanged. This does not attribute the
timing failure to candidate selection.
