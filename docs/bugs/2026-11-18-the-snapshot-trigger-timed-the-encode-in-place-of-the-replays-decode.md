# The snapshot trigger timed each record's encode in place of the replay's decode (2026-11-18, A-420; A-243's open line; §4.8)

Found by the cadence mix test's CI failure (run 44260193629, the macOS gates job) and left open in GAPS by its fix
(`docs/bugs/2026-11-18-the-cadence-mix-test-held-the-snapshot-decode-to-the-budget.md`): the commits' measure of a
tail's replay and the replay itself diverged by machine and moment, about 1.7x over on a Mac Studio and 0.4x to 1.25x
on CI's runner. The supervisor's order (2026-11-18): the trigger must rest on the replay's real cost, failing test first.

## Description

A partition snapshots once the replay work its commits measured since the last snapshot reaches the recovery budget
(A-243, `slates_db::replay::snapshot_due`). A recovery keeps to the budget only while that measure is at least what
the replay spends. The commits timed each record's encode, checksum, copy into the ring and apply. A replay reads,
checksums, decodes and applies, and it decoded the whole tail into one list before applying any of it. The decode was
never timed: where a record decodes dearer than it encodes, the tail replayed in more than its commits measured, and
the trigger let it run past its budget.

## Root cause

`Db::mutate` and `Db::append_timed` timed `LogRing::append` (encode, checksum, copy) as the stand-in for the replay's
read, checksum and decode. The encode and the decode differ in cost by record shape: a decode allocates every owned
field (a key, a name, a list), an encode copies its bytes into one buffer. `LogRing::replay` also collected every
decoded record (`Replayed::ops`) before `derive_partition` applied the first, a tail's worth of live allocations that
no commit had made.

## Evidence

- The failing unit test on main 6d060076d with the test alone (kiln r95, the Mac Studio d1323898, load 15, debug):
  64 records each setting a volume's signers to 1,024 one-byte keys (331,520 bytes); the commits measured 5,705,791 ns
  of replay work; a recovery replayed them in 8,494,000 ns past a 181,000 ns baseline, noise 34,000 ns: 1.49x the
  measure.

## Fix (A-420)

- `crates/db/src/record.rs`: `LogRing::encode` makes an `Encoded` body; `Encoded::decode_as_replayed` decodes it as the
  replay will, refusing `Corrupt` before anything is written if it does not decode (a replay would cut the record and
  every record after it); `LogRing::write` writes it (header, checksum, copy). `LogRing::replay` hands each record to
  its caller's apply before reading the next and returns the count (`Replayed::records`).
- `crates/db/src/replay.rs`: `append_record` times the encode, the decode and the write together; `Db::mutate` adds the
  apply, `Db::commit`'s record adds them to the transaction's timed applies; `derive_partition` applies as it walks.
  The measure now covers the replay's work and errs early only by the encode (which bounds the replay's read: an
  allocation and a write of every byte against an allocation, a zeroing and a copy) and a preempted commit's stall.

## Runs

- Red: above. Green: the same test on the fix (see the progress note of g251; kiln on the Mac Studio).
- Filed for main: `snapshot_cadence.rs` (the three cadence tests and the mix test), `snapshot_cost.rs`, `model.rs` and
  `publication.rs` of slates-db, and the provisioning bench for the commit's added decode.

## Open

None in this record. The trigger's input is now the replay's own steps; a recovery in a colder process than the
commit's (its caches, a page's first touch of the mapping) is the machine's, and the mix test's recoveries judge it.
