# A burst on a live volume shipped the whole volume (2026-11-17)

Contracts: A-200 "Backpressure (AC-8.9)" (entries not yet durable are bounded by a credit derived from the measured
data; a publication past it waits; nothing is queued without bound), AC-8.9, R3 (no hardcoded tuning number), §4.10.

## Description

A live-shipped volume written by several clients at once shipped a copy of its whole state to every holder, though each
write changed one page. The owner's credit was a fixed two entries (`live_ship::CREDIT = 2`, with `RETAIN = 4`, both
R3 debt the module doc named), and a publication the credit turned away was not made to wait: it left the volume owing a
whole record (`LiveVolume::owes_whole`), so the volume's next publication was appended as its whole image, every inode
and every byte of content, sent to every holder, and every writer behind it waited for that copy to be durable.

Every barrier publishes (each recorded verb, an NFS `COMMIT` or stable `WRITE`, an `fsync`), so any workload with more
than two barriers inside one holder round trip on one volume crossed the credit: many agents editing one volume, a
build's parallel writes, a burst after a holder's stall. The bigger the volume, the more each burst cost.

## Root cause

`Shipper::append` refused an entry once `in_flight() >= credit` (a count), and `live_ship::published` answered the
refusal with `refuse(state, id, "fleet.live.refused.no_credit")`, which sets `owes_whole`; the next publication then
built `whole_record` (the volume's `to_image` with the content of every extent) and appended it with
`append_checkpoint`. The count was a placeholder for A-200's derivation ("Little's law over the measured apply rate and
holder round trip"), which was never built; and a count bounds nothing that matters, since an entry may be a whole
record, while the cost of a refusal is the volume's whole size.

## Impact

Bandwidth and latency only, never safety: a whole record is a correct entry, and every writer still waited for
durability. But on a large volume a burst of writes cost a copy of the volume per burst on the owner and on the wire, the
writes queued behind it, and the holders' copies churned through whole records.

## Exact edits

- `crates/db/src/live.rs`: `Bounds { credit, retain }`, in wire bytes of entries (`Entry::wire_bytes`), replaces the
  `(credit, retain)` counts. `append` refuses only when entries are in flight and their bytes with this one's would pass
  the credit (one entry is always admitted with nothing in flight); `append_checkpoint` (and `admits_checkpoint`) admits
  a whole state while what is in flight is within the credit, so at most the credit and one whole state are in flight.
  The bytes in flight are kept apart from the retained tail (`unsettled`, settled in `refresh_durable` as durability
  advances), since a checkpoint past the credit can trim the tail below them. The tail is held to `Bounds::retain` bytes,
  never dropping the newest entry for size. `set_bounds` replaces `retain_up_to`. `Bounds::of_whole` states the
  derivation: both bounds are the bytes of the whole state an install would send.
- `crates/server/src/live_ship.rs`: `CREDIT` and `RETAIN` removed; a new shipper starts at `Bounds::default()` (its first
  entry is the whole record, admitted with nothing in flight), and every whole record and install image sets
  `Bounds::of_whole` of its size; `owes_whole_within_credit` asks `admits_checkpoint`.
- `crates/db/tests/live.rs`: the model and the unit tests state their bounds in bytes (`bounds_of`, the same regime as
  the counts for their one-size entries); three new tests: a burst within the whole state admitted and one past it
  waiting, the credit counting each entry's bytes and never stopping the log, and the tail held to its bytes.
- `crates/server/tests/fleet.rs`: `a_burst_of_writes_to_a_live_volume_ships_its_changes_not_the_volume`, the failing test.
- `crates/cluster/tests/live.rs`, `crates/cluster/examples/replication_bench.rs`: the bounds in bytes.

## Evidence

- The failing test, three in-process daemons at `f = 1`: a half-MiB volume live-shipped and written once; both holders
  held off their only shard for a quarter of a liveness window while sixteen clients each write half a page (this Mac,
  debug, load 33–56, 2026-11-17; logs `~/.cache/slates-scratch/g230-logs/burst-red-1.log`, `burst-fix-2.log`):
  - main's credit code (a copy of the tree with `crates/db/src/live.rs` and `crates/server/src/live_ship.rs` from
    origin/main 48c64720): 13 of the burst's 16 publications turned away (`fleet.live.refused.no_credit` 0 → 13), two
    more whole records on the log (`fleet.live.whole` 1 → 3, each a copy of the half-MiB volume to both holders), 4
    entries appended (1 → 5), at most 3 in flight; red at "no publication of the burst was turned away for credit";
  - the fix: none turned away (0 → 0), no whole record (1 → 1), an entry for every write (1 → 17), 16 in flight at
    once; every write answered `FsDone`; green.
- `cargo test -p slates-db`: every test passes, the nemesis model's included (byte bounds in the same regime as the
  counts), 2026-11-17, this Mac.

## Siblings swept

Every other fixed count on the live path: `RETAIN` (removed with the credit); the holder's `hold_bound` (twice the
volume's size limit, derived from its catalog); the transport's per-message budget (`exchange_budget`, derived from
the bytes and the session). No other literal bounds the log.
