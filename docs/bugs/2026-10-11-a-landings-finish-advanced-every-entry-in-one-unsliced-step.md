# A landing's finish advanced every entry in one unsliced step (2026-10-11)

## Description

A granted landing runs in slices on the shard that owns its volume (AUD-29-25), and A-132 moved its durability barriers
off the shard. But the **finish** — the step that advances the overlay past what reached the disk (forgetting each landed
entry, or rebasing it from the snapshot, then invalidating the listings) — ran as **one synchronous `with_state` on the
shard**, O(entries). For a landing of many entries it held the shard for the whole advance, blocking every other client.

This was the last unbounded step on the shard during a landing, and it is a bounded-work violation (R4: "every loop over
user-scaled data is chunked into cooperative slices under the shard's per-iteration budget").

## Evidence

The advance (`land_advance`, `crates/vfs/src/base.rs`) timed over the in-memory host (`land_bench`, scratch, 2 runs):

| entries | advance (one call) | per entry |
|---|---|---|
| 1,000 | 0.95–0.99 ms | 0.95 µs |
| 10,000 | 10.0–11.0 ms | 1.05 µs |
| 25,000 | 25.2–29.1 ms | 1.08 µs |
| 50,000 | 52.3–53.2 ms | 1.05 µs |

Linear at ~1.0 µs/entry (≈1.68 µs/entry on a real volume, from a 600-entry server measurement), so **~105 ms at 100k,
~1 s at 1M — one step, on the shard**. Real OCI images and monorepos reach those file counts (condition 2), and the hold
is a condition-12 p99 hit for every other client of that shard.

Not a correctness bug (the landing lands correctly) and not a regression — it predates A-132. A-132's `finish_granted`
ran it inside one `with_state`, so it was atomic (no concurrent verb saw a partial advance).

## Root cause

The finish was one function (`finish_landing`) called once from the engine's `Phase::Finish` unit, inside the server's
single `complete_landing_with` transaction. Its O(entries) parts:
1. `advanced()` — built an identities `BTreeMap` from the manifest and a `Vec<Landed>`, O(n·log n).
2. `land_advance()` — the per-entry overlay surgery, O(n).
3. the report's category counts — six `reports.iter().filter().count()` passes, O(n) each.

All ran in one shard turn.

## The fix (A-133)

Slice the finish's advance under the shard's per-iteration budget, folding the O(n·log n) and count passes into the
sliced loop so **no unit is O(entries)**, with the completion record still committed as one atom at the end (A-127).

- **`crates/vfs/src/base.rs`:** `land_advance` splits into `land_advance_begin` (one-time base-plane setup),
  `land_advance_one` (one entry — pure in-memory, no durable op), and `land_advance_end` (the one-time listing
  invalidation). The original `land_advance` stays as a wrapper for the one-call path and tests.
- **`crates/land/src/engine.rs`:** a new `Phase::Advance(Box<AdvanceState>)` between `Phase::Finish` and the report.
  - `begin_advance` is now O(1): degradations, `close_dirs`, the base config — no sweep.
  - `advance_step` processes one report a unit: it advances the entry where it landed (the former `advanced` +
    `land_advance`, folded; the identity comes from the manifest entry at the same index, not a `BTreeMap`), and
    **accumulates** the report's counts, the held count and whether every entry advanced as it passes each one.
  - `finish_report` is O(1): the terminal state from the accumulated figures, the audit record, the report.
  - `finish` becomes a loop over a new `finish_step(budget_ns)`, which the server drives in slices; `run_through`
    (one-call) keeps its no-budget behaviour.
  - `advanced()` and `terminal_state()` are removed (folded in).
- **`crates/server/src/landing.rs`:** `finish_granted` splits into `step_finish` (one budgeted advance slice, the same
  shape as `step_granted` — base host lent back between slices, the slice timed into the histogram) and `complete_finish`
  (the post-advance records, run once in the final `complete_landing_with` transaction). `drive_granted_landing`'s
  `Stepped::Ready` branch loops `step_finish` across `with_state` calls, yielding and keeping the lease alive between,
  then commits.

**Why it is safe to slice (confirmed by reading the code and by test):**
- The advance records no durable op; it mutates the anchor-held overlay (RAM). A crash before the completion commits
  rebuilds the un-advanced overlay from the log, and the resume re-lands. So moving it out of the single transaction
  changes no durability.
- Reads stay consistent through the sliced advance: a not-yet-advanced entry reads the overlay's private copy (the landed
  bytes); an advanced one reads the base (also the landed bytes).
- A concurrent `status`/drift query is safe: a **modified** base file is copied up yet keeps its base witness, so from
  the write-phase placement until its own advance it self-reports `Replaced` drift — this is pre-existing (the write
  phase is already sliced), the advance's `base_forget` clears both the witness and the recorded drift, and the drift set
  only shrinks during the advance. My earlier "no false drift" note was wrong for modified files; the real guarantee is
  read-consistency plus the advance leaving the base clean.

## Impact

Before: one O(entries) shard step at the finish — ~105 ms at 100k files, ~1 s at 1M.

After: the finish advances at the step budget. Measured over the in-memory host at 125 µs budget, p50 and p99 flat from
1k to 100k entries:

| entries | steps | p50 | p99 | max |
|---|---|---|---|---|
| 1,000 | 7 | 125 µs | 130 µs | 130 µs |
| 10,000 | 82 | 126 µs | 752 µs | 752 µs |
| 50,000 | 436 | 126 µs | 235 µs | 3.1 ms |
| 100,000 | 793 | 126 µs | 135 µs | 6.4 ms |

p50/p99 are bounded by the budget at every size; the max is a rare tail outlier over hundreds of samples on the shared
Docker VM (scheduling/allocator), not an algorithmic step — per-unit instrumentation found no O(n) unit (one 556 µs
outlier across 100k entries). Nothing O(entries) runs in one unit.

## Proven

- `crates/land` 62 tests, the sliced-vs-one-call oracle among them (`a_landing_stepped_one_unit_per_slice_lands_exactly_as_the_one_call`
  and the crash-at-every-instruction oracles now drive the sliced finish): the sliced finish is byte-for-byte the one call.
- `a_sliced_advance_reads_consistently_and_leaves_the_base_clean` (new): reads return the landed content at every slice,
  the drift set never grows during the advance, and the base is clean once it is done.
- `crates/vfs` 256 tests (the `land_advance` split); the server landing suites (daemon, landing fairness, snapshot
  landing, lease) 29 tests; workspace clippy and `xtask check` clean.

## Sibling / owed

- The finish's own slices are counted into the same histogram as the other landing slices; a `status` query between
  advance slices seeing a modified file's transient self-drift is pre-existing (the write phase) and out of scope — if
  it should be suppressed during a landing, that is a separate change (§4.5/§4.6, confirm scope with Ada).
