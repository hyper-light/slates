# The shard's operation headroom exceeded its arena capacity, so every volume was refused

Date: 2026-09-16
Area: `crates/server/src/daemon.rs` (`build_shard`, the operation-headroom derivation)
Severity: on a host where the derivation tips over — a moderate arena capacity with a large seatable
client count, which the GitHub `macos-latest` (arm64, 16 KiB pages) runner hits — the daemon refused
**every** volume with `BudgetExceeded { available: 0 }`, even a 1 MiB one. It presented as a flaky
per-daemon failure across the macOS gates and SDK lanes (a different daemon-spinning test each run).

## Symptom

macOS CI lib/client/SDK tests flakily panicked creating a bounded volume:

```
called `Result::unwrap()` on an `Err` value: Refused(BudgetExceeded { available: 0 })
```

It did not reproduce on the dev box (macOS arm64, but far more RAM) or in Docker under any memory,
CPU, or rlimit constraint — every input to the reserve derivation is stable, so the boot profile
alone could not explain it.

## Root cause

Named by a one-shot store-budget diagnostic added at the refusal site (`report_first_budget_refusal`,
`crates/server/src/verbs.rs`). The first refuse-all logged:

```
config[reserve_per_shard=1252698794 shards=2 …]
bytes[capacity=1073741824 committed=0 retained=0 headroom=2003828736]
```

The byte-budget admittable is `capacity - committed - retained - headroom`, so with a **headroom of
1.87 GiB against a capacity of 1 GiB** it saturates to 0 and refuses everything. The derivation was
sound; the **operation headroom was larger than the arena it sits in**.

The headroom (§4.2, "the bounded temporary coexistence of in-flight operations") is `2 × chunk_bytes
× concurrent_writers`, for a copy-on-write copy-up's source and destination chunk. It set
`concurrent_writers = clients_per_shard` — a copy-up reserved for **every client the shard's RAM
could seat** (hundreds). But a copy-up exists only while a write is *in flight*, and a shard admits at
most `requests_in_flight_per_shard` operations at once (its Little's-law admission limit, ~20). So the
reservation over-counted by the ratio of seatable clients to in-flight operations (here ~477 / 20 ≈
24×). The ratio of headroom to capacity is independent of the machine's total RAM (both scale with the
reserve), so once it exceeds one it refuses on every host of that shape — the dev box escaped only by
having a much larger arena for the same ratio to sit under. macOS arm64's 16 KiB base page (vs 4 KiB
on the Linux/x86 lanes) enlarges `chunk_bytes`, which is why the tip-over showed on the macOS runner.

## Fix

Two changes in `build_shard`:

1. **Bound the concurrency correctly.** `concurrent_writers = min(clients_per_shard,
   requests_in_flight_per_shard)` (the in-flight limit carried as `config.caps.attachments`). Concurrent
   copy-ups cannot exceed the shard's concurrent in-flight operations, so this is the true structural
   bound — it drops the headroom ~24× in the failing case (1.87 GiB → ~84 MiB), leaving ~942 MiB
   admittable.
2. **Cap the headroom within the capacity (D-12 "degrade and keep serving").** `headroom =
   min(raw, capacity - chunk_bytes)`, so a shard can never reserve its whole arena and refuse
   everything, on any host — at least one chunk window always stays admittable.

## Verification

- The store-budget diagnostic named the exact breakdown on the `macos-latest` gates run (`5bd9d35`):
  `capacity=1073741824 headroom=2003828736`.
- `cargo test -p slates-server --lib` (87/87), `-p slates-client --test client` (4/4) and `--test
  consumer` pass on the dev box after the fix (they passed before too — the box never reproduced it).
- The macOS gates and SDK lanes are the acceptance test; a regression re-refuses and the diagnostic
  re-prints the breakdown (kept in place as permanent §4.14 observability).

## Sibling sweep

- `clients_per_shard` is used elsewhere for genuine per-client sizing (the client slab
  `Slab::new(segment_slots, clients_per_shard)`, the segment-slot fan-out) — those are correct and
  unchanged; only the *headroom* conflated "seatable clients" with "concurrent operations".
- The reserve derivation (`region_bytes`, `effective_capacity`) is unaffected — it was never the
  collapse; the boot profile's `reserve_per_shard` was healthy throughout.
