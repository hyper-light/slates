# tests: a parking test demanded a measured wake for a kick the design does not time

**Status:** fixed 2026-10-23 (approved by Ada in plan mode); root cause confirmed from CI's logs, the design, and a
reproduction under TSan here. **Design:** §4.3 (the shard's online wake estimate); `crates/rt/src/parking.rs`.
**Found by:** the TSan job of CI runs 39572843785 (2026-10-21, `f3a15f50`) and 39914027363 (2026-10-23, `b80aa5f6`).
Both failed at `crates/rt/src/parking.rs:540`, in `parking::tests::a_kicked_park_learns_its_kicks_stamp`.

## Description

The test parks a timed shard and has another thread kick it once the park is announced. It then asserts
`woken.latency_ns().is_some()`, "the stamp is this park's, none left by the untimed one". Both failures printed the
park's record:

```
Woken { announced_ns: 269424026624, waiting_ns: 269424031553, returned_ns: 269424146963, kicked_ns: Some(269424029367) }
Woken { announced_ns: 509870286166, waiting_ns: 509870288921, returned_ns: 509870318966, kicked_ns: Some(509870287684) }
```

In both, the kick's stamp falls after the announcement and before the wait's entry (announced < kicked < waiting).

## Root cause

That window is the design's `Woken::early`. `park_unless_pending` reads the wait's entry time just before it waits, "so
a stamp before it is a kick the wait will find pending". `latency_ns` returns `None` for such a stamp, because timing a
wake that never happened would bias the estimate low. The shard counts it as early and never measures it
(`crates/rt/src/shard.rs`).

So the test asserted more than the design promises. Its message names its real intent: the stamp belongs to this park,
and was not left by the untimed park before it. A stamp in this park is either early or measured, and never stale. The
loom model of the same protocol (`crates/rt/src/parking.rs`, the `loom` tests) already asserts exactly that: never
`stale()`, with measured wakes counted for non-vacuity.

**Why only under TSan.** The kicker spins until the park is announced, then kicks at once. Instrumented code widens the
gap between the announcement and the wait's entry, so the kick lands inside it more often. Both the window and the
assertion date from 2026-09-25 (`2c76bcbd`, `8126ad2a`).

## Impact

The TSan lane failed spuriously in 2 of the 99 TSan jobs in the last 100 runs on main. The runtime itself behaves as
designed: an early kick is counted, never timed.

## Exact edits

- `crates/rt/src/parking.rs`:
  - **The test's assertion:** the stamp is not stale, and is either early or measured.
  - **A new deterministic test**, `a_kick_landing_while_the_park_is_set_up_is_its_own_and_untimed`, of the early
    case. The kicker kicks once the park is announced, and the park's re-check returns only after the kick. The stamp
    must then be this park's (not stale), early, and unmeasured, and the wait finds the kick already sent.
- `docs/wip/GAPS.md`, row AUD-29-31–32: one sentence naming this record.

## Verification

| | before | after |
|---|---|---|
| the new test asserting the old rule, plain | failed 10 of 10, each an early stamp | (the design's rule) passed 50 of 50 |
| `a_kicked_park_learns_its_kicks_stamp` under TSan on this Mac (aarch64, nightly 2026-09-04, load 8 to 11) | failed 2 of 200, both early stamps | passed 200 of 200 |
| the new test under TSan | | passed 50 of 50, no report |

- **Miri:** the parking tests pass, with the two timed ones ignored as before (a timed park reads the boot clock).
- **Plain:** the parking tests 5 of 5.
- **Gates:** workspace clippy `-D warnings`, fmt, `cargo xtask check`.

## Siblings

**A search for `latency_ns()` across `crates/` found no other test demanding a measurement of a single kicked park:**
- **`parking.rs` lines 478 to 483:** classify synthetic stamps, which is deterministic.
- **Line 442:** the loom model, which counts measured wakes rather than demanding one.
- **`shard.rs` line 1362:** the runtime, which takes `None` through `let … else` and counts the early and the stale.

**Already skipped:** the client's `a_client_learns_its_wake_from_the_parks_a_reply_ended` counts how many parks
measured a wake. It is a timing-dependent count of the same kind, already skipped in the TSan lane with its reason
(`xtask/src/tsan.rs` `SKIPPED`).
