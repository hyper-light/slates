# A volume a publication left out lost its logged writes at the next restart (2026-11-18, §4.8, A-63, A-422)

## Description

A write that a FUSE mount, a Shards stream or slates' own virtio-fs device acknowledges goes into the shard's write log
before its reply (A-63, A-316, A-410). A restarted daemon replays the log over the volumes its image rebuilt, but only
when the log's stamp equals the committed image's generation. A publication emptied the log and stamped it only when it
captured every volume (`verbs::clear_write_log`: `if !captured_every_volume { return; }`). The reasoning, in its comment,
was that "a volume skipped keeps the log, so its writes stay replayable".

They did not stay replayable. A publication that cannot image a volume leaves it out: an overlay whose base host is away
(a landing holds it) or whose base cannot be read. Such a publication still commits a newer generation for every other
volume, and the log kept its older stamp. Suppose the daemon then died before a publication that captured every volume.
Its successor found the log's stamp older than the committed image, which `WriteLog::read` takes to mean "that image
already carries its writes", and replayed none of it. The left-out volume's committed record is older than those
writes, so every write acknowledged to it since its last capture was gone. Whole closed files could go: while the log
holds every write, a FUSE close's flush and a stream's skip their own publication (A-399). Nothing reported the loss,
since a stale log counts as nothing lost.

Found by reading on 2026-11-18 (g268, sweeping the write log after A-410), then proven by the test below.

## Proof

`a_volume_a_publication_leaves_out_keeps_its_logged_writes_for_the_next_daemon` (`crates/server/src/verbs.rs`):
- A shard holds a plain volume and an overlay of the server crate's sources, each with a new file, published.
- One base file of the overlay is copied up, so its next publication must be its whole image.
- Both files take a write, logged as a transport logs one before its reply.
- The overlay's base host is taken away, as a landing holds it.
- A delta is published: it carries the plain volume and leaves the overlay out.
- The log is opened as a restarted daemon opens it, against the generation the journal committed.

With the fix, the restarted daemon is handed exactly the overlay's write. The plain volume's write is not handed back,
since the delta carries it and replaying it could undo a later change. With the fix removed (no restamp owed, no
records kept), the restarted daemon is handed nothing, and the test fails there: red-checked once on a Mac Studio
(2026-11-18T20:16Z, debug), `left: []` against the overlay's record.

## Fix (exact edits)

- `crates/server/src/write_log.rs`:
  - `WriteLog::retain` keeps the records of the volumes a publication left out, in their order. It works in place, front
    to back, then stores the length that covers them, the layout and the new stamp, in that order. It scrubs what lies
    past the kept records, as `clear` does (A-99).
  - The overflow word gains a second bit, `RESTAMP_OWED`. `owe_restamp` sets it before a publication that leaves a
    volume out commits; `retain` clears it once the records are kept, and `forgive_restamp` clears it when the
    publication is refused.
  - A successor that finds a stale stamp with that bit set reports the writes lost. It then starts its log under the
    recovered stamp with the loss kept, so a further restart reports it too, until a publication captures every volume.
    With a current stamp, `read` takes bit 0 alone as the overflow.
  - A span the content object refuses during a retain leaves no record covered and the overflow marked, so a half-moved
    log is never replayed.
- `crates/server/src/verbs.rs`:
  - `settle_write_log` replaces `clear_write_log`: it clears the log when every volume was captured, and retains the
    left-out volumes' records otherwise.
  - `publish_checkpoint` and `publish_delta_into` owe the restamp before a commit that leaves a volume out, and forgive
    it when the commit is refused.
- Tests: four new unit tests. Three are in `write_log.rs`: the retain keeps records in order and scrubs the rest; a
  restamp owed at a death is reported lost, and is replayed when the death came before the commit or the publication
  was refused; an overflow stays reported through a retain. The fourth is the shard-level test above.

## Siblings (from reading; not fixed here)

- A checkpoint leaves a volume it cannot image out of the image entirely, where a delta keeps the volume's committed
  record. Recovery then refuses that volume (`RecoveryIncomplete`, AUD-05) until a later publication captures it whole.
  So content acknowledged at earlier publications is out of reach after a death in between.
- `verbs::replay_one` replays a logged write with `Volume::write`, without the overlay's host. A logged write into an
  overlay's base file that the image has not copied up is then applied without its copy-up. What the volume does with
  it is to be checked.
- When the log is full, `log_write` (FUSE, streams) forces a publication. It counts the write as logged only when that
  publication left nothing out (`published.skipped.is_empty()`), so a captured volume's write, which the image carries,
  is still marked as an overflow and reported lost. Since A-422 the retain also frees room, so the write could be
  appended again.
