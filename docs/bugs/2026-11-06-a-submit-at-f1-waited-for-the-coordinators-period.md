# A submit at f = 1 waited for the coordinator's period

Found 2026-11-06 by goal 4's first Linux rung on adacachy (AMD Ryzen 9 9950X3D2, Linux 7.2.9-2-cachyos). The setup
was loadtest-scale's `crates/cli/tests/load_scale.rs` at `bbbaddd9` merged with `18033dd5`: three server processes at
`f = 1` and one client node, `kernel/` of Linux v7.4-rc1, 1,000 clients, `--per-connection 32 --hot-files 800
--reply-patience 60 --rounds 3`, in a capped unit. Every version committed, but no submit was answered as accepted.
970 were answered `Overloaded` past the 1 s liveness budget, and the oracle failed on each, since the harness books
`Overloaded` as a refusal. Log: `~/.cache/slates-agents-scratch/fixtures/rungs/1k-clean.log`.

§4.16 "Commit" (AUD-11) answers a submit at `f > 0` only once its version's merge record commits at the quorum. The
record plane ran only on the control shard's coordinator, once a period (`HEARTBEAT_NS`, 100 ms). Each period it puts
a version's inputs, and only in a later period ships its record. So every answer waited out at least one period, and
usually two. On a green with many submitters the answers queued, one version a period per holder.

## Failing test

`a_submit_at_f1_is_answered_without_waiting_for_the_coordinators_period` (`crates/server/tests/fleet.rs`). Its setup
is a settled three-node fleet at `f = 1` and twenty works over one green, submitted one after another, each timed.

It expects:
- every submit accepted as the next version;
- the median answered within one coordinator period;
- no holder counting `merge.inputs_unheld`.

Before (debug build, adacachy, 2026-11-06): 20 of 20 answers took 208 to 647 ms, median 237 ms. The least was two
periods.

## Cause, from the coordinator's own lines

Temporary prints logged each enqueue, each merge pass and each put or ship with whether it got sessions. With the
record plane woken on each enqueue (below), the answers fell to a few milliseconds, but one run in two still had
stretches near 100 ms. Two holders were in play:

1. **The fast holder's late acknowledgement.** A version's input put returns once `f + 1` hold it, so the second
   holder's reply is a straggler, and its late acknowledgement is discarded (`LateReplies::Discard`). When that holder
   was the one the record had to ship to next, the re-put found its session still out (`dispatch=false`). Nothing woke
   the plane again, and the version waited for the period.
2. **The lagging holder released the inputs it needed next.** `put_inputs` put each version's inputs to every
   candidate that lacked them, including a holder still owed an earlier version. A holder keeps only its newest
   placement ahead of its accepted records (`crate::content_retention`, which assumes one placement in flight per
   object). So v6's inputs released v2's, and the holder refused v2's record `merge.inputs_unheld`. The owner put v2's
   inputs again and shipped again, the next version's put released them again, and so on until a period realigned it.
   Two runs of the test counted `merge.inputs_unheld` 53 and 71 times, and `fleet.content.released` 14 and 18 times,
   on the lagging holder.

## Exact edits

- `crates/server/src/merge_service.rs`:
  - `nudge_record_plane` marks the control shard's record plane nudged and wakes the coordinator. It is called when a
    version is enqueued (`enqueue_record`), when a version's inputs are newly acknowledged (`put_inputs`), and when a
    record is newly acknowledged with versions still pending (`record_merge_acks`). Only an event that makes new work
    nudges, so a holder that never answers costs no pass beyond the period's own.
  - `nudged` is the future the coordinator waits on. `MergeShardState::nudged` and `nudge_waker` hold the state.
  - `put_inputs` puts a version's inputs only to the candidates whose next record is that version (`MergeWork::PutInputs::to`).
- `crates/server/src/fleet.rs`: `pace_running_merges` waits out the coordinator's period and runs the merge plane at
  once on each nudge. The council, the root, the seals and the heads keep the period. The merge plane stays on the
  coordinator, which owns the owner's acceptor and the holders' sessions.
- The test above. §4.16 status, GAPS, and A-235.

## Verification

After (debug build, adacachy, 2026-11-06): 5 of 5 runs passed, with medians of 2.7, 3.9, 4.4, 3.3 and 3.7 ms. The
slowest answer in any run was 42.7 ms. `merge.inputs_unheld` and `fleet.content.released` stayed at 0 on every node.
With the nudge alone and the put still made to every candidate, the medians were 3.0 to 85.6 ms, and 1 of 5 runs
failed at 102.7 ms.

## The test judges in the daemon's terms (2026-11-07)

The failing test first held the median answer under a wall bound of one coordinator period (100 ms). Under load that
bound measured the machine, not the claim: live-fixes saw a median of 909 ms under 36 spinners on a Mac, where every
period and every exchange stretch alike. The claim is that an answer does not wait for the coordinator's period, so
the test now asks the daemon itself.

- The coordinator counts its periods (`MergeShardState::coordinator_period`).
- The record plane stamps each pending version with the period it first works it in (`PendingMergeRecord::first_period`).
- When a version places at the quorum, the owner counts it `merge.placed_within_period` if that is the same period, and
  `merge.placed_after_periods` if a period began first.
- The test expects every version judged, and most placed within their period.

Red check, with A-235's nudge disabled (the coordinator only sleeps out its period, as before A-235): 1 within, 20
after, answers about 202 ms each. Green on `4811191a`: 21 within, 0 after, quietly and under two spinners a core
(4 CPUs, 8 busy loops, 3 of 3 runs, median answers 285 to 342 µs).
