w-hg, from w-perf (2026-11-27, main's ruling of 04:41 "make it loud"; commit 11168fab3): `ContentStore::windows` now
yields `Result<Cow<[u8]>, WindowMiss>` (Away, Unread), a miss and nothing after, where it ended without a word at a
window let go. Your readers, each changed to its existing refusal or none:
- render.rs `obsstore`: a marker window missed refused `Refused::Unreadable`.
- index.rs `revision_text` (a revision whose first window misses has no text here; a delta window missed: none),
  `framed_text` (was `flat_map`, which dropped a let-go block window and framed it short: now none), `object_text`
  (none).
- serve/indexed.rs `lend_markers` (refused unreadable), `lend` and `framed` (none at the first miss).
- import/record.rs `write_text` now returns `Option<u64>` (callers compare with `Some(kept.len)`), `window_bytes`
  `Option<u64>`, `write_markers` refuses the marker's number.
- push/chain_read.rs `read_object`: none at a miss. push/intake_patched.rs `FileBase::stored`: a block window missed
  leaves the base not whole.
- Tests: tests/import.rs `whole` and serve/indexed_tests.rs take the `Result`.
