# A full log's forced publication reported a captured volume's write as lost (2026-11-18, §4.6 "Linux", A-63, A-436)

## Description

Every write a FUSE mount or a Shards stream acknowledges is appended to the shard's write log before its reply
(A-63, A-316). A log with no room for a write forces a publication instead, which carries the write, since the volume
already took it. The transports counted that write as logged only when the publication left no volume out:
`publish_shard(s).is_ok_and(|published| published.skipped.is_empty())`, in `fuse::log_write`, `shards::log_write`
and, for a `FORGET`, `write_log::log_forgets`.

A publication can leave out a volume it cannot image (an overlay whose base host is away). When it did, every write
that met the full log was counted unlogged, including writes to volumes the publication had captured, whose image held
them:
- the log was marked overflowed;
- the next daemon reported every file a taken-over mount had open, or a resynced stream's guest had open, as having
  lost writes (`LostWrites`);
- while the overflow stood, a close's flush published instead of trusting the log.

A write to the left-out volume itself was marked as an overflow too, though the publication had just given back room
for it: since A-422 a publication that leaves volumes out keeps only their records, dropping those the image carries.
For a `FORGET`, the same rule counted a captured volume's `FORGET` as unlogged (`write_log.forget_unlogged`).

Found by reading on 2026-11-18 (g268, A-422's sibling sweep), then proven by the tests below.

## Proof

Both tests are in `crates/server/src/verbs.rs`. Each runs on a shard with a reduced reserve, so its write log is 8 MiB.
The shard holds a plain volume and an overlay that cannot be imaged (a base file copied up, its host taken away), and
the log is filled with the plain volume's records until it refuses one.
- `a_full_logs_forced_publication_keeps_a_captured_volumes_write_and_reports_no_loss`: a write to the plain volume's
  file, logged as a transport logs it. The forced publication leaves the overlay out and captures the plain volume;
  the log must not be marked overflowed, and a restarted daemon must be told nothing was lost.
- `a_full_logs_forced_publication_appends_a_left_out_volumes_write_again`: the same with a write to the overlay's own
  file. A restarted daemon must be handed that write and told nothing was lost.
- Red-checked once on a Mac Studio (2026-11-18T21:00Z, debug) with the old rule put back: both tests failed at "the
  log is not marked overflowed", after asserting that the forced publication left the overlay out.

## Fix (exact edits)

- `crates/server/src/write_log.rs`:
  - `keep_record` settles a record a full log refused by its own volume: kept when the forced publication captured the
    volume; appended again when it left the volume out (A-422's retain gave the room back); unkept only when the
    publication is refused, or the log is still full.
  - `log_acknowledged_write` marks the overflow, counted, only for a write nothing kept.
  - `log_forgets` keeps its `FORGET`s the same way.
- `crates/server/src/fuse.rs` and `crates/server/src/shards.rs`: each `log_write` calls `log_acknowledged_write`, where
  each had its own copy of the old rule (the stream's counted a refused overflow mark and the mount's did not; now both
  do).
- The two unit tests above.
