# A shard's content keys were never let go (2026-11-05, A-208)

## Description

A shard's content cipher (`crates/server/src/content_cipher.rs`, `ShardCipher`, A-99) derives a key for each volume
it seals, and registers a key for each one a recovery image names. It kept every key in a vector that nothing ever
removed from, not even at the volume's destroy, and it found a key by a linear scan of that vector. Its module doc
said the opposite: "Bounded: the keys held are those of the epochs some chunk still names, at most one per chunk
record."

## Measurement

- `crates/client/tests/cipher_keys.rs`
  (`a_create_and_destroy_loop_leaves_the_shards_keys_where_they_started`): on a one-shard daemon, 64 volumes created,
  written with a mebibyte (so their content seals) and destroyed left **64 keys held against a floor of 0** (macOS
  arm64, 2026-11-05, load about 100). The test waited its whole 64 s bound for the count to come back.
- `xctrace` (Time Profiler) on `provision_bench`, macOS arm64, 2026-11-05: over the histogram's ~440,000 creates,
  `ShardCipher::register` held **24,052 of the 26,986 samples** inside the server's `verbs::create`; 934 of every
  1,045 of them were its own instructions (the scan), and the key derivation under it (`derive_child`) 81.

## Root cause

Two things were missing. Nothing let a key go, because the cipher was never told when the last thing naming it was
gone. And keys were found by position in the vector, which the cipher scanned because it had no index.

## Impact

Every daemon: one key's memory per volume ever created (hyper-seal's `VersionKey`, an AES-256-GCM key schedule, and
its 32-byte identity), never returned, and a create whose cost grew with every volume the shard had ever made. The
provisioning histogram's later rows (8 and 64 clients, then the parked form after ~440,000 creates) measured the scan.
On a two-core runner where A-208 leaves one shard, every key sat on that shard, and the parked p50 rose 2.3–2.5×
(diag run 41992744368).

## Edits

- `crates/vfs/src/content.rs` (minimal and additive, the content write path): `ChunkCipher` gains `hold`, `release` and
  `keys_held`. `ChunkStore::seal` and `adopt_chunk` hold a sealed record's key before inserting it, and let the hold
  go with a refused record. `free_chunk` and `give_back_chunk` release it after the record is removed.
  `ChunkStore::cipher_keys_held` reports the count.
- `crates/server/src/content_cipher.rs`: each key carries its hold count, an identity-to-slot index replaces the scan,
  and a key goes with its last hold, its slot reused by the next new key. `key_for_volume` holds the key once for the
  volume's life, and `release_seal_key` lets that hold go. The module doc states the bound the code keeps.
- `crates/server/src/verbs.rs` (`release_slot_credits`, where a destroyed or discarded volume's slot leaves): the
  volume's hold is released. A refused release is counted as `content.key_release_refused`.
- `crates/server/src/daemon.rs`: `Daemon::content_keys_held`, the observation the daemon-level test reads.
- The other `ChunkCipher` implementations (the test ciphers and `sealed_read_bench`'s) gain the three methods.

## Tests

- `a_create_and_destroy_loop_leaves_the_shards_keys_where_they_started`: red before (64 against 0), green after
  (11.24 s).
- `a_key_is_kept_while_a_chunk_sealed_under_it_lives_and_goes_with_its_last` (`crates/vfs/tests/content_in_place.rs`):
  a volume sealed in one life is recovered in the next, where its new key differs; its old chunks must read back under
  the old key, the old key must go when a rewrite frees them, and nothing may be left after the destroy. Red before (the
  recovered read refused, since nothing held the old key on the chunks' behalf). Red-checked after the fix: with
  `free_chunk`'s release removed, 2 keys stay where 1 should; with `adopt_chunk`'s hold removed, the read is refused
  again.

## Siblings found

- `publish_delta_into` (`crates/server/src/verbs.rs`) re-walks every volume of the shard at each mutating verb (GAPS,
  open). Not changed here.
