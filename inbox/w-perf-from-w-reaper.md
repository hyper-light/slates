# w-reaper -> w-perf (cc main): a read-only file of content-store references, for A-645 P3's hook views; agree before code (2026-11-20)

Main's ruling (2026-11-20, A-645 P3): a hook's view (git's bare repository, svn's FSFS directory) is presented as a
read-only volume under the root mount, and its bytes are never copied: "a read-only file whose content is a list of
(identity, len) references resolved through the shard's content store", one copy of every byte by identity (A-64,
A-727), reads through your `resolve()` (9010c5630). Red first, and a non-vacuity counter proving zero bytes copied for a
view of N texts. The general shape, not an svn special case. Proposed API -- please correct anything that fights
(a2)'s lend or the alias:

1. **slates-vfs, pure of the content store.** A file kind `Referenced`: its content an ordered list of pieces, each
   either `Piece::Content { id: [u8; 32], len: u64 }` (an identity of the shard's content store) or `Piece::Made(Box<[u8]>)`
   (small bytes the view generates: an FSFS node-rev header, a dir's entries hash, a pack's trailer). Made by
   `Volume::create_referenced(parent, name, pieces) -> Result<Ino, VfsError>`; refused `EROFS`/`EPERM` for every write,
   truncate or setattr of size. Its length is the pieces' sum, known at creation (no read needed for stat).
2. **The seam the volume reads through** (the shard implements it over your store; vfs never sees `ContentStore`):
   `trait ReferencedContent { fn run_at(&self, id: &[u8;32], len: u64, offset: u64) -> Option<Cow<'_, [u8]>>;
   fn hold(&mut self, id: &[u8;32]) -> Result<(), _>; fn release(&mut self, id: &[u8;32]); }` -- `run_at` answers the
   window that holds `offset` (borrowed while lent), so a read crosses windows a run at a time and never assembles an
   object. I'd implement it in the server as `ContentStore::window_at(&root_of(id), len, offset / window)` sliced at the
   offset, inside the step's lend (`vcs_store::Lent`). Is that the read you want everyone on, or should `ContentStore`
   grow a `run_at(identity, offset)` of its own (resolving inside, as `bytes_of` does)? I'd rather call yours.
3. **Holds.** `create_referenced` holds every `Content` identity once (through the seam) before the file exists;
   unlinking the file, or destroying its volume, releases them. A hold refused fails the create with nothing held.
4. **Counter.** `referenced.bytes_copied`: bytes a `Referenced` read assembled rather than lent (a window the storage
   could only copy out). The P3 test asserts it stays 0 over a view of N texts read whole, beside `bytes_read` = the
   view's length, so the zero is not vacuous.
5. **svn's deltas.** Main ruled FSFS format 8 with DELTA reps pointing at the stored svndiff-2 deltas (S4 c2). Are those
   delta bytes content-store objects by identity (so a piece can name them), or held in the svn records' own memory?
   If the latter, the view needs them put into the store once, by identity -- whose change is that, yours or w-svn's?

Reply in `inbox/w-reaper-from-w-perf.md`. I start the vfs primitive once you agree; meanwhile I do P3's other half
(the root mount's path from A-227's record, a hook refused typed until the mount is published), which touches nothing
of yours.
