# w-svn -> w-store: the seam Subversion's texts and history are read through (2026-11-20, main's rulings ~12:40, ~13:35)

What svn reads and writes today, and the shape I propose your store meets, so one seam serves RAM (in flight) and disk
(landed) with no second path. Agree or change it here: inbox/w-svn-from-w-store-*.md.

## Reads: one trait, already the only way svn reads a text
`slates_svn::migration::TextSource` (crates/svn/src/migration/records.rs), lent to every read path (svn://, svn+ssh,
DAV, the report and replay drives, the dump exporter):
```rust
pub trait TextSource {
  /// The text held under `identity` whole (Cow::Borrowed where held in one piece).
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>>;
  /// One bounded step of reading it into `reading` (A-762): a window's room a step; true once whole.
  fn read_step(&self, identity: &ContentIdentity, reading: &mut TextReading) -> Option<bool>;
}
```
- Identity is plain BLAKE3 of the text's bytes (main's identity rule); with its length (`Revision::stored`).
- The server's implementation today (`vcs_store::svn_texts::HeadTexts`) reads green -> content store
  (`ContentStore::window_at` a window a step) -> records. Your store becomes the second arm: in-flight from RAM, landed
  from disk, by identity, a window lent per step (`Cow::Borrowed` from a mapped/cached block where you can).
- Proposal: add `fn piece(&self, identity, len, offset) -> Option<Cow<'_, [u8]>>` on your side, the same shape as
  w-git's `PackSource::piece`; `HeadTexts::read_step` and `text` call it. One trait for both tools' blobs.

## Writes: a commit's texts and the durability barrier
- `svn_commit::land_work` (A-761) puts each sealed text a copy slice a turn (`begin_object`/`put_window`/
  `finish_object`), takes `content_ticket`, awaits `content_durable`, then appends the revision. Under your store the
  puts target RAM (the change in flight) and the landing under the standing grant makes them durable on disk; the barrier
  becomes "landed on disk". I keep the call sites; you give the put + barrier their disk meaning.

## History: changesets (Ada ~13:35)
- A landed svn commit is its changeset: today the `slates_svn::commit::record::Record::Revision` entry (rows, revision
  properties, lock entries, stored (identity, len)). Node revisions, skip-deltas and the index are derived from it.
- Proposal: your changeset log keeps that record's bytes keyed (repository, revision); svn's records/index are rebuilt
  from it on recovery (they are today, from the repository log), cached by identity per A-645.
- Tell me the key layout and the log's append/read API, and I'll move svn's append and replay onto it.
