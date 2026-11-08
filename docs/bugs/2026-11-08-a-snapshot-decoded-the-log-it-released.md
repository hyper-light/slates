# A snapshot decoded the log it released

**Found** 2026-11-08, chasing polls of about 130 ms in `presentation_fairness.rs` (which turned out to be another
function, below). A profile of the granted test showed `Db::snapshot` at 72 samples, 71 of them in `LogRing::trim`.

**Root cause.** A maintenance snapshot runs inside the commit that crosses its cadence, on the shard's thread.
`Db::snapshot` publishes the partition, then called `LogRing::trim(segment, next_seq)` to release the log. `trim` walked
every record since the last snapshot through `record_at`: it copied each body into a new buffer, copied it again to
checksum it, and decoded it, only to learn the record's length and reach the tail. Its one caller always released
everything, so all of that work was thrown away. The snapshot is written from the partition, so nothing the log held
could change the result. A damaged dead record also made the walk return `Corrupt`, which refused the snapshot and left
the log unreleased.

The cost is the interval's. A fresh daemon's interval was 1 MB; since A-243 a snapshot waits until the tail's measured
replay work reaches the recovery budget, so the walk could take about that budget in one poll.

**Red** (`crates/db/tests/snapshot_cost.rs`, `a_snapshot_releases_the_log_it_covers_without_reading_a_record`: sixteen
records past a snapshot, one byte of the first flipped, another snapshot): `Corrupt { seq: 8, reason: "checksum
mismatch" }`.

**Fix.** `LogRing::release_all` stores the sequence base, the head and the tail, and reads no record. `release_stores`
is that order as data, read by the code and by the crash test.

**Green.** `Db::snapshot` over N records logged since the last one, the partition held at 8 volumes
(`a_snapshots_cost_against_the_log_it_covers`, ignored; this Mac at load 31-80):

| records | release, before | release, after | debug, before | debug, after |
|---|---|---|---|---|
| 1,000 | 253 µs | 23 µs | 5.1 ms | 87 µs |
| 10,000 | 2.9 ms | 17 µs | 193 ms | 88 µs |
| 100,000 | 76 ms | 35 µs | 1.28 s | 87 µs |
| 1,000,000 | 654 ms | 28 µs | 14.9 s | 79 µs |

**The crash ordering.** `a_crash_at_every_store_of_a_release_recovers_the_partition_the_snapshot_holds` restores the
ring's words as a crash after each prefix of the three stores leaves them, on a ring whose head is at the first byte
and on one whose head is past its first record. Recovery equals the live partition at every point, replays nothing, and
a later mutation survives a second recovery. `the_records_after_a_release_are_what_a_recovery_replays` is the same
release with no crash. Checked by mutation: the three orders that store the tail before the head fail (a tail at zero
under a later head reads as an empty ring whose next records sit behind the head), the three that store the head first
pass, and a release that leaves the tail fails. A sequence base one short passes, because `append` rewrites the base on
an empty ring, so it is an equivalent mutant.

**What this did not explain.** This was first suspected of the polls of 100 to 164 ms of the shard's own CPU in
`presentation_fairness.rs`. It was not them. With only this fix, 30 of 124 named waits still held 100 ms or more
(before: 17 of 62), with a lower ceiling (129 ms against 164). A per-poll CPU timer then named them directly:
`return_dead_recovery`, in `reap_loop`'s tick (see `2026-11-08-a-landings-plan-held-its-shard.md`).
