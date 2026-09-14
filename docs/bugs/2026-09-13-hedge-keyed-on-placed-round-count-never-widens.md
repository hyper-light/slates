# The content hedge never widened when the first round's only holder was unavailable — and a progress witness was born "advancing"

Date: 2026-09-13
Area: `crates/server/src/fleet.rs` (`content_work`, `put_seal_content`; the §4.10 hedge landed the same
day on `agent/content-replication`), `crates/cluster/src/progress.rs` (`ProgressWitness`).
Severity: the hedge — the design's remedy for a slow first-round holder — silently did not fire in the
one case it exists for. Found integrating the content-replication branch: its by-use test
`a_slow_first_round_candidate_is_hedged_after_the_measured_p95` failed 1 in 3 singly (placed after
**3.217 s** against a 3 s hold, vs ~350 ms) and once in the suite.

## Two defects

### 1. The hedge was keyed on the count of rounds that *placed*, not on the clock

`put_seal_content` chose its targets by `if work.round == 0 { remote.take(f) } else { remote }`, and
`job.rounds` advanced only when a round produced a placement. When the first round's sole target (the
first remote candidate in rendezvous order — the held one) had **no record session available** (borrowed
by the previous seal's straggler dispatch, whose holder task runs the full span while that holder's
shard is starved), `take_sessions` returned nothing, the round dispatched nothing, produced no
placement, and the count stayed 0 — so every following period aimed at the same unavailable holder.
The trace, from the failing run:

```
TRACE-ROUND round=0 targets=[H2662…] took_sessions=[] deadline_ns=10044416 span_ns=1100000000   ×9 periods
TRACE-ROUND round=0 targets=[H2662…] took_sessions=[H2662…]        ← the hold ended, session back
TRACE-ROUND round=0 returned_after_ms=57 outcome=err
TRACE-ROUND round=1 targets=[H2662…, H1481…] took_sessions=[H1481…]
TRACE-ROUND round=1 returned_after_ms=20 outcome=placed
```

The hedge delay (p95 = 10.04 ms) had elapsed after the first period; `content_work`'s gate opened
correctly; the *target selection* never widened. The design's trigger is time outstanding since the
first attempt ("hedged to the remaining candidates after the measured p95 put latency"), and a holder
whose session is unavailable is exactly a slow holder.

**Fix.** `ContentWork.hedged: bool`, decided by the clock in `content_work` (`first_round_at_ns`
outstanding ≥ the hedge delay), and `put_seal_content` selects through the pure `hedge_targets(hedged,
remote, f)`; `round` remains the healer's bookkeeping. Unit test
`the_hedge_widens_the_targets_on_the_clock_not_the_placed_round_count`.

### 2. A progress witness that had never advanced reported "progressing" for a stall window after birth

`ProgressWitness::new` seeded `last_advance_ns = now`, so `is_progressing` was true for a whole stall
window with **no** observed advance. For `content_budget` (stall window = hedge delay, lookahead 1/1)
that meant a round with zero acknowledgements at the p95 was granted its extension unconditionally.
Found while hunting defect 1; it was not defect 1's cause (the by-use test still failed 1/5 with only
this fixed — recorded), but it contradicts the extender's stated rule ("granted only to a round still
gathering") for every caller. **Fix.** `last_advance_ns: Option<u64>`, `None` until the first real
advance. Unit test `a_witness_that_never_advanced_is_not_progressing` (failed before, passes after);
the extender's five existing tests unchanged. `DispatchWait::judge(gathered, now)` split out of
`keep_waiting` so the round-level rule is testable without a clock
(`a_round_with_no_acknowledgement_at_the_hedge_delay_expires_rather_than_extends`).

## Wrong turns, kept on record

The witness was my first attribution and it was wrong as the *cause*: the round-level unit test I
wrote for it passed on the unfixed code, which should have stopped me sooner. The trace, not the
reading, found defect 1. Two hours earlier the same lesson (`…churned-target-dir.md`).

## Validation (2026-09-13)

- By use, before: 1/3 then 1/5 then 1/2 failing singly (3.164–3.388 s placements), load 5–7.
  After: **10/10** (349–971 ms — the p95 varying with the prior seal's readings, the derived behaviour;
  the 3 s cliff gone). `cargo test -p slates-server --lib fleet::tests` 7/7;
  `cargo test -p slates-cluster --lib progress::` 6/6.
- Full suite and gates: see the merge commit.
