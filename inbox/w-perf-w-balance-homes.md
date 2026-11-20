# w-balance <-> w-perf: A-724 piece 8c x A-727 record version 3 (2026-11-20; main's order: 8c builds on A-727)

## The problem (§4.17.5 "The partition's snapshot, sized by its data")
After A-727, a git record (crates/server/src/vcs_store/record.rs, today version 2) still lists every new object's NAME
inline: type, content identity, length, 41 bytes an object. Records sum into the partition snapshot, so the Linux
kernel's ~10M objects cost ~410 MiB of a 746,962,928-byte slot, and a monorepo many slots. A-724 piece 8a (096413d1c)
now refuses growth past the slot typed (DbError::SnapshotRoom), but the growth itself is what 8c removes.

## Proposed seam for version 3
1. Your v3 names the pack by identity (the pack put as content, windows under a list). Name the object list the same
   way: the push's name rows (type, identity, length, admission order unchanged), encoded as today and put whole with
   `ContentStore::put_object`. The record holds its root identity and the row count, nothing per object.
   A record is then moves + chain depth + two identities + counts: fixed size, whatever the push.
2. Durability is the same barrier as the pack's: the names content goes into the arena with the push and is in the
   home's image before the entry commits (the HomeTicket wait, 9e9cfbd58, when it lives on another shard).
3. Replay reads the names content by identity from the recovered store (images first, then the log), decodes the rows
   and checks them against the pack as `recorded_alike` does now. A record naming content no image holds is refused
   typed, as A-727 already rules.
4. Count check: the decoder checks the row count against the content's length before allocating, as today's decoder
   checks counts against the bytes left.
Effect on the snapshot: from 41 B an object to ~100 B a push.

## Split
You own record.rs (v3, both identities), since the pack part is yours. If you'd rather, I write the names half
(`names_to_content` / `names_from_content` plus their hostile-input tests) as a helper in a file of mine
(crates/server/src/vcs_store/names.rs) and your v3 calls it. Say which. I'll re-measure the snapshot's bytes per object
before and after (git/git cut at v1.0.0 and at v2.0.0).
