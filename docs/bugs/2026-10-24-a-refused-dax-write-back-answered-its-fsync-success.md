# A refused DAX write-back answered the guest's FSYNC success (Shards part 3, step 3)

- **Date:** 2026-10-24
- **Subsystem:** the Shards route's ingest (`crates/server/src/shards/dax.rs`, `ingest_step`, `compare_and_write`;
  `crates/server/src/shards.rs`, `ingest_turn`, `serve_request`), as committed in `8f0e1b2b`.
- **Severity:** correctness. A guest's `fsync(2)` or `close(2)` returned success while its stores through its DAX map had
  not all reached the volume, so the guest believed bytes durable that were not.
- **Found by:** reading the ingest's refusal arm while adding step 4's base-file rules, then proven by
  `a_write_back_the_volume_refuses_fails_the_fsync_that_asked_for_it` (`crates/server/tests/shards.rs`). The test fills a
  bounded volume but for a MiB, stores two MiB through a sparse file's map and syncs. It failed before the fix (the
  `FSYNC` answered 0, expected `ENOSPC`) and passes after.

## Symptom

`FSYNC` answers 0 after a write-back the volume refused. The refusal was counted (`shards.dax_ingest_refused`) and
nothing else changed.

## Root cause

When the bridge refused a write-back read or write (no room, the file unreadable after drift, the attachment revoked),
`compare_and_write` and `begin_inode` mapped the refusal to `()`, dropping its errno. `ingest_step` counted it and moved on
to the next inode, and `ingest_turn` then served the held request as if the ingest had succeeded: an `FSYNC` reply of 0.

## Fix

- The ingest keeps the errno of its first refused write-back (`Ingest::refused`).
- `serve_request` takes it (`Ingested { wrote, refused }`) and, after serving the request (so a `FLUSH`'s own work
  happens), replaces the reply of an `FSYNC`, `FLUSH` or `READ` with that errno (`shards_fs::refuse`), as a refused
  barrier already replaces a reply with `EIO`.
- A `RELEASE`'s answer the kernel ignores, and a `FORGET` has none; their refusals stay counted.
- The guest's stores stay in its map, so a later `FSYNC` compares again and writes back what still differs, once there is
  room.

## Sibling sweep

- The stream's end: its ingest's refusal is counted and the stream ends; no request is left to answer.
- The refill added in the same change (step 4) keeps a range's bytes when the volume refuses to read it (counted
  `shards.dax_refill_refused`). The guest learns of the refusal from its next request of the file, which the volume
  refuses too.
