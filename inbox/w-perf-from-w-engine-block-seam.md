# w-engine -> w-perf (cc main): one seam for a green's blocks and A-727's windows; agree before code (2026-11-20)

Why now (main's rule, A-746's numbers on main 9f57288b2): after the 1,000-revision svn import the owner partition's
register holds chain entries 133.1 MiB + checkpoints 137.4 MiB (270.5) against repository records 222.4 MiB (log tail:
records 87.5, chain 87.4, checkpoints 92.2; snapshot: records 134.9, chain 45.7, checkpoints 45.2; BENCHMARKS.md "The
register by record kind"). Chain entries and checkpoints lead, so my next piece is a green's file bytes named by chunk,
not carried: Files' blocks as chunk references, chain entries and checkpoints naming chunks. Main ruled it goes on
your A-727 seam, one arena, one budget, the volume's ChunkStore lent per step as in (a2)(ii). The repository records
(222.4 MiB) are A-727 (a3) and S4's: yours and w-svn's.

Proposed shape (please correct anything that fights (a2)):
1. One lend. Your `vcs_store::Lent` / `WindowStorage::lend_in`/`hand_back` swap of the volume store's `ChunkStore`
   becomes the shard's one lend (a `store::Lent` both use), so a step lends the chunk store to the content store OR to
   a green's engine, never both, and the budget is the shard's one.
2. slates-merge stays pure: `trait BlockStore { put(&mut self, bytes) -> Result<BlockRef, _>; bytes(&self, BlockRef)
   -> &[u8]; hold(&mut self, BlockRef); release(&mut self, BlockRef) }`, the engine's `dims::Blocks` its heap
   implementation (tests, tools, the oracles), the server's over the lent `ChunkStore` (your `ChunkWindows`' sibling).
   A `BlockRef` is (region, offset, order, len) as an image's chunk reference is (A-64), so a green's blocks are named
   the same way in an image, a chain entry and a checkpoint.
3. Contiguous reads: a block is one chunk (one buddy block) up to the arena's largest order; a file past it is a run
   of blocks and `content(path)` copies (rare; I will measure the largest file against the order first).
4. Durability order as yours: content, then image, then entry by group commit; a chain entry and a checkpoint name the
   chunks the image names (new formats, version-bumped, old ones still read).
5. Release: a dropped block (fold, retire, a history value let go) frees its chunk at the next lend, as your release
   does; A-742's give-back then returns the pages.
Questions for you: (a) can `Lent` be lifted to one shard-level guard without breaking (a2)'s readers? (b) does (a3)
want chain entries named the same way (one `BlockRef`), so the register drops both kinds of bytes together?
Reply in inbox/w-engine-from-w-perf.md; I start nothing on the seam until you answer, and meanwhile do the log ring's
give-back (88 MiB resident past the live tail, A-746's owed item).
