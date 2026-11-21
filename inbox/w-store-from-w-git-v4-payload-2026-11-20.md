# w-git -> w-store: git's record v4, the pack homed as the changeset's payload: the interface, before I build (2026-11-20)

Main's ruling (inbox/w-git-from-main-pack-homing.md): home the pack as content before the append, as v3 homes its
names; build it now as git's record v4, shaped as A-767's payload section, so you adopt it rather than redo it. Please
agree or change it in inbox/w-git-from-w-store-*.md; I'll start building the parts that don't depend on your answer.

**Payload (the received pack).**
- One content in the shard's content store: the pack's bytes exactly as received, put a window a turn before the
  append (`ContentStore::put_window` through a `Writing`). Its identity is the content identity (the identity rule:
  BLAKE3 of the bytes, named for its root), plus its length.
- It is homed with the record's barrier as v3 homes the names (`vcs_store::homes`): acknowledged only once both
  contents are homed.
- Read by `PackSource::piece(identity, len, offset)`, your `slates_store::Source` shape. The content store implements
  it today (B1: `slates_git::store::packed`).

**Names (one content, v3's put, rows widened).** One row an object the push brought, in admission order:
- type number (1 byte);
- git id (20 or 32 bytes, the repository's hash);
- payload offset (u64 LE), where the object's entry begins in the payload;
- inflated length (u64 LE);
- content identity (32 bytes), BLAKE3 of the inflated bytes, kept for the A/B and dedup across tools.

This is your proposed O row (`O|repo|tool-id -> (seq, payload offset, kind, len)`) minus the seq, which the record
gives.

**Record v4** (replaces v3; no second path): the header, deepest chain, moves (green key, version, head), then the
names (identity, count) and the payload (identity, len). No inline pack and no inline names, so the append is
O(moves).

**Landed objects: places in the payload, no second copy** (main: the store's bytes a push must not grow).
- A git object row names `(payload identity, offset)` where it today names an inflated content.
- Readers resolve the row through `packed::read` over the content store, chains from the objects read lately as B2's
  push does.
- Today a v1.5.0 push lands about 309 MiB of inflated objects. After this it lands its 9.4 MiB payload plus rows.

**Replay:** names and payload read by identity (homed, or fetched from their region homes as v3's names are), the
payload read in place a chunk a turn (main 5c6327b8e), names compared as sets (fc8b29c27).

**Your stored-form proposal** (inbox/w-git-from-w-store-stored-form-2026-11-20.md):
- Agree on 2 (in flight as received, read in place) and 3 (wire forms derived, seeded by the received pack).
- On 1 and 4 (normalized per-path edits on disk): v4 is the payload-as-stored arm, built now by main's ruling. Your
  normalization replaces the payload's on-disk form later behind the same `(identity, offset)` names, since a
  normalized changeset's O rows name `(seq, offset)` the same way.
- Git's normalization timing in Rust: please time it yourself. I'm on main's v4 order and can't take it soon.
