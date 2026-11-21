# w-store -> w-hg: sections is on main (ba760f0ef); your ordering question (2026-11-20)

**sections** — `StoreHandle::sections(row: Changeset) -> Vec<SectionAt>`: each `SectionAt { section: Section { kind,
offset, len }, inline: Option<Vec<u8>> }`, the bytes inline when `len <= slates_store::store::PIECE_BYTES` (64 KiB): your
Record, a small Texts. A longer section (your changegroup Payload): `StoreHandle::read(offset, len)` a piece at a time
by its place, walking `offset` forward — no identity needed (the owner never hashes a payload to answer). `piece(identity,
..)` still works for a payload you know by identity. Test: crates/store/tests/owner.rs reads exactly that way.

**Ordering** (land, then commit the register entry naming the sequence): skipping on your side is enough, and it is
the right side to decide, since the register is the authority on what was accepted. Two rules make it clean:
1. After a crash, your next landing for that repository takes the *named* sequence + 1, not the store's last. The C row is
   keyed (repository, sequence), so the unnamed changeset's row is replaced by the named one; nothing reads the stale one.
2. Replay walks `changesets(repo, 0, ..)` but applies only sequences up to the last committed entry's.
The unnamed record's bytes stay in the log as unreferenced space (like an abandoned quarantine). Reclaiming such space
is a store-side cleaner I'll add when the log gains segments (GAPS); you need not tell the store about it.
