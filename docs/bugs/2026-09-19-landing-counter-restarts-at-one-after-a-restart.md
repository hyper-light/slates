# The landing counter restarts at 1 after a restart, so recovered landings block new ones

**Date:** 2026-09-19. **Found as a sibling** of the attachment-counter bug fixed under AUD-01
(`2026-09-19-mount-capability-attachment-dies-with-its-client-and-the-daemon.md`, "Siblings"): the same
shape, one counter over.

## Description

A landing id is `landing_id(partition, counter)` (`crates/server/src/verbs.rs`) with the counter
`LandingState::next_landing`, which starts at 1 in `LandingState::default()`
(`crates/server/src/landing.rs`) on every boot. A presented or finished landing writes a durable
`LandingRecord` under that id (`Op::LandingRecorded`), the records survive a restart (they are in the
partition snapshot and the log), and the guard refuses a duplicate id (`Partition::check`:
`AlreadyExists`). So on a partition that recovered N landing records, the first N `land` verbs after a
restart are refused typed — each refusal still advances the counter (`present` and `finish` increment
before the record write), so the N+1th succeeds. The refusal reads as a database refusal, not as what
it is: a counter that forgot the records it recovered.

## Root cause

Recovery rebuilds the partition's records but not the shard's id counters: `next_landing` (and, until
AUD-01, `next_attachment`) is process state initialized to 1 and never seeded from the recovered
partition. Any id counter whose records are durable must boot past them.

## Fix

`Partition::highest_landing()` (`crates/db/src/partition.rs`) and
`verbs::next_landing_counter(partition)`: one past the highest recovered landing counter, or 1 for a
partition holding none; the shard's `LandingState` is seeded with it at boot
(`crates/server/src/daemon.rs`), the same treatment the attachment counter received.

## Failing test first

`crates/server/tests/recovery.rs::a_landing_presented_before_a_restart_does_not_block_the_first_landing_after_it`:
a landing is presented on the first daemon (no grant: `GrantRequired`, the record written), the daemon
is stopped and a second started over the same anchor segment, and a landing is presented again — it
must be `GrantRequired` with a **new** landing id, not a refusal. Before the fix the second present was
refused `AlreadyExists` (the recovered record's id re-minted).

## Validation (this box, 18 cores, 2026-09-19)

- Failing first: with the seed disabled, the test fails at the second present with
  `Refused(AlreadyExists { existing: VolumeId { … } })` — the recovered landing's id re-minted. With
  the seed: `cargo test -p slates-server --test recovery` **5 passed, 5.44 s**.
- `cargo test -p slates-server --test daemon` (the landing and grant flows): **9 passed, 15.37 s**.
  `cargo clippy` (server, db; all targets, `-D warnings`), `cargo fmt --check`,
  `cargo xtask check`: clean.

## Siblings

- `next_prefix` (inode prefixes) is already seeded from the recovered images (`rebuild_recovered`,
  "hand out prefixes past every recovered one"); `next_attachment` was fixed under AUD-01. No other
  per-shard id counter mints ids for durable records: grant ids are the landing's, consumer ids are the
  control shard's durable enrollment records (`ConsumerRecord`) — checked, seeded from the records.
