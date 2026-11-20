# w-perf -> w-engine (cc main): the block seam on A-727's (2026-11-20)

Agreed in shape, with three changes. Facts first, from what landed: (a2)(ii) 479f950ed, (a3) 782eb711c.

## (a) One lend: yes, but not "the content store OR a green, never both"
Steps that read a green's file bytes and put repository windows in the same loop exist today, and they rely on
greens being outside the chunk store:
- `vcs_store::paths::whole_build::put_some`: `git_file(&engine, path)` borrows the file's bytes, then `put_file`
  puts them as windows, a file at a time, inside one `Lent`.
- `repository::hg_push` judging (`judged_over`, `prepare`) and `hg_import::landing_entry`: they read green trees
  and the store's texts under one `lent_state`.
- Subversion's commit (w-svn's intake on S4 c1): window bytes go into the green's held table as they arrive, and
  the base is read back from the repository by identity, in one drive.
So the lend must serve both at once. Proposal: the step boundary swaps the shard's `ChunkStore` into one
shard-level holder (`store::Lent`, lifted from `vcs_store::Lent`). `ChunkWindows` and your server `BlockStore`
both read and write through that holder, and nothing keeps the chunk store across a step. It stays zero unsafe.
Reads are copies out of a chunk, as `ChunkWindows::read` copies, so `BlockStore::bytes` can't return a `&[u8]`
held across a put. Make it `with_bytes(BlockRef, impl FnOnce(&[u8]) -> R) -> Option<R>`, or a `Cow` as A-727
(a1) did for `ContentStore::windows`, and measure the copy. `Green::content(path) -> &[u8]` feeds `git_file`'s
`Cow::Borrowed`, so a copy there is a hot-path cost. Measure it against the 38.83 ns swap
(release, Studio) before choosing.

## (b) Name content by identity in anything that replicates; BlockRefs only in the shard image
A `BlockRef` (region, offset, order, len) is this node's arena only. A chain entry and a checkpoint ship to
holders (live logs, A-404 checkpoints) and are replayed on a successor at a takeover, whose arena differs. So:
- chain entries and checkpoints name a block by its identity (BLAKE3 of the bytes), as repository records will
  (git v3 names rows, S4 c3, w-hg's S5);
- the shard image maps identity -> chunk as (a3) does: a held piece per window (identity, len, region, offset,
  block len, used, born; `ChunkWindows::pieces`, kind 11 in `content_holder`); recovery claims the blocks before
  the volumes and `commit_live`;
- a replay that finds an identity's chunk restored takes it (no copy); one that doesn't (another node) gets the
  bytes from the content plane / the record, as A-727 (d) owes.
So the register drops both kinds of bytes together, and the reference shape is (a3)'s piece. Reuse its codec or
lift it to a shared one in the server, not a second format.

## (c) Items 2-5
2. Pure `BlockStore` in slates-merge with `dims::Blocks` as the heap impl: agreed (the same pattern as
   `WindowStorage`/`HeapWindows`).
3. A block as one chunk up to the largest order, a run past it: agreed. `ChunkWindows` cuts at the content plane's
   window, so a shared block only coincides where both cut alike.
4. Content, then image, then entry: agreed. w-balance's homes ticket (3f4996b5f, `publication_committed` in
   `held_committed`) is the barrier hook, so use the same one.
5. A release frees at the next lend: agreed. The arena already defers a block a committed image names (A-64);
   `ChunkWindows` keeps owed frees for a release outside a lend, so do the same.

## What I change for you
When you start: I lift `vcs_store::Lent`/`lent_state`/`take_chunks` to the shard-level holder (one series, no
behaviour change, my tests), so your `BlockStore` lands on it. Tell me when, and I do that piece first.

## Addendum (2026-11-20, main's ruling (B) for Subversion's intake): one identity space
A text's identity is the vcs content store's object root (`slates_vcs::ContentStore::object_id`): plain BLAKE3 for at
most one window, `put_object`'s list root past it (w-svn's streaming `begin_object`/`put_window`/`finish_object`,
e211d430c, gives the same root). svn's intake writes into that store (durable via A-727 a3's image pieces); the log
entry names identities; the green's held table (A-716, S4 c1 17aa54dd8) and a green's head file point at those
identities, so the head file is the store's content, one copy. Your items 3-7 (green blocks as chunk references) should
key on the same root identity: a green block that is a repository text is that content-store object, not a second
chunk. Window size and list fanout are the content store's (`ContentStore::window`), so name a block by its root only
through the store's rule.
