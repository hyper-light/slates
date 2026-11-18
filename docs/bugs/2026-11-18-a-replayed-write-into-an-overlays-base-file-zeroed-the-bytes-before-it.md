# A replayed write into an overlay's base file zeroed the bytes before it (2026-11-18, §4.6 "Linux", §4.15, A-63, A-429)

## Description

A write that a FUSE mount, a Shards stream or slates' own virtio-fs device acknowledges after the shard's last
publication is kept in the write log, and a restarted daemon replays it over the volumes its image rebuilt (A-63). The
bridge wrote an overlay's file through the overlay's host (`VolumeBridge::write`, `host_for`). There
`Overlay::write` copies a base file up first: it reads the file from the disk and records its witness, and for the large
class it pins each touched window from the disk. Only then does it apply the bytes. The replay (`verbs::replay_one`)
wrote through the volume alone, `Volume::write`, with no host.

A file the dead daemon had copied up after its last publication is still a base file in the recovered image (its body
`Body::Base`, nothing pinned). There the replayed write opened its chunk window from the window's start with nothing of
the disk in it (`Volume::open_window`, `ChunkStore::open`), so every byte of the window before the write read as zero
where the disk's bytes had shown. The write itself landed, but the file around it was wrong: a one-byte edit to the
middle of a source file blanked everything from the start of its window up to the edit. The file was also left without
the witness its copy-up records. So the rest of it kept following the disk with no drift check, as an untouched base
file's bytes do, though it had been written.

Found by reading on 2026-11-18 (g268, A-422's sibling sweep of the write log), then proven by the test below.

## Proof

`a_replayed_write_into_an_overlays_base_file_keeps_the_disks_bytes_around_it` (`crates/server/src/verbs.rs`): a shard
holds an overlay of the server crate's sources. A logged write of "REPLAYED" at byte 10 of its `lib.rs`, which nothing
has copied up, is replayed as a successor replays it, and the file is read back through the overlay. With the fix the
file reads as the disk's `lib.rs` with those eight bytes in place. Red-checked once on a Mac Studio (2026-11-18T20:29Z,
debug) with the replay writing through the volume alone: the first difference was at byte 0 (`left: Some(0)`), the ten
bytes before the write read as zeros.

## Fix (exact edits)

- `crates/server/src/verbs.rs`: `replay_one` writes an overlay's record through `Volume::with_host` and the slot's host,
  as the bridge does, and refuses `BaseUnavailable` for an overlay whose host is away. Recovery reopens an overlay's
  host before the replay runs (`build_recovered_volume`), so in a recovery only a volume it could not rebuild lacks one,
  and that volume is not replayed at all.
- The unit test above.

## What remains

A base file the disk changed between the dead daemon's copy-up and the replay is copied up as the disk holds it at the
replay. The dead daemon's own copy was never published and is gone, so the replayed write lands on the newer bytes.
That is the most the log can give back without logging a copy-up's bytes, which would cost a whole file's bytes per
first write.
