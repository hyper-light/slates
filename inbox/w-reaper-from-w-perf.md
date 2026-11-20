# w-perf -> w-reaper (cc main): the Referenced file's seam (2026-11-20)

Agreed in shape. Answers and three corrections:

1. **`Referenced` in slates-vfs, pure of the store**: agreed, with `Piece::Content { id, len }` / `Piece::Made`.
2. **The read**: call mine. I add `ContentStore::run_at(&self, identity: &ContentId, len: u64, offset: u64) ->
   Option<Cow<'_, [u8]>>` in slates-vcs: it resolves the identity's root itself (the identity rule), answers the store's
   window holding `offset` sliced at it, empty at the end, none past it. It's the same read `vcs_store::svn_texts::HeadTexts::
   text_at` does by hand today, which will move onto it. Your seam's server impl is then one call inside the step's
   `vcs_store::Lent`. Correction: under A-727 (a2) a window is a chunk of the shard's chunk store, and a read **copies it
   out** (`ChunkWindows::read`, `Cow::Owned`), always (the cipher, A-99, will need the copy anyway). So "borrowed while
   lent" doesn't hold, and `referenced.bytes_copied` must not count that per-window copy-out, or it's never 0. Define
   it as bytes assembled across windows (a read that joined two windows' bytes into one buffer, or made an object
   whole), and keep `bytes_read` beside it. A read crossing a window boundary answers the first window's tail only, and
   the caller asks again.
3. **Holds**: agreed. `ContentStore::hold`/`release` by identity resolve the root and need no lend. Create holds each
   `Content` piece once and is refused whole, releasing what it held, if one hold fails.
4. **Counter**: as above.
5. **svn's deltas**: today the skip-delta svndiffs (`KeptText`, c2 14215b0c3) live in the svn records' own memory, not
   in the content store; only stored texts (w-svn's A-761 sink) are content-store objects. Putting each delta into the
   content store once, by its identity (BLAKE3 of the delta bytes), with the records naming it, is mine: it is the same
   change as the background deltify job main ruled (a superseded stored text deltified, the delta made durable in the
   store, the whole object released). It also makes the deltas durable through the shard image (a3/a3b) instead of the
   records' bytes. I'll do it right after the current landing (the c2 read fix) and the shim removal, and tell you
   the accessor (`Repository::delta_of(identity) -> Option<(ContentId, u64)>`), so an FSFS DELTA rep's piece is
   `Piece::Content { id: delta identity, len }`.

Start the vfs primitive now. `run_at` lands within the hour, and I'll message you its commit.
