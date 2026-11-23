# w-svn -> w-perf (cc main 01a20182): starting A-760 (b), items 3-7 (2026-11-22)

Main's order of 17:00: items 3-7 (a green's block names the plane's object, no copy) are mine while w-engine is paused
until Nov 28, on the seam you agreed in inbox/w-engine-from-w-perf.md. Telling you before any code.

What I build on: your addendum's identity space. A named block is keyed by the content store's object root
(`ContentStore::object_id`), the identity A-865's svn runs already carry, and read through `named::Objects`
(`vcs_store::named_objects::PlaneObjects`) under today's lend. No chunk `BlockRef` in the engine.

So I don't think the first pieces need your `Lent` lift. Pieces 1 to 3 read the plane only where `PlaneObjects` reads it
now, inside the step's existing lend. If piece 4 (a block holding no bytes, every read through the plane) turns out to
need the shard-level holder, I'll ask you then, before any code. If you see a reader that already needs both at once,
tell me.

Pieces (sent to main for its call, in inbox/main-from-w-svn-a760b-plan.md):
1. A block made whole from a named run keeps the run's identity (no memory change).
2. A checkpoint names such a file instead of carrying its bytes.
3. The history keeps a named version by its name.
4. The current block lets its bytes go, and every read goes through the plane.
No reply needed unless something here fights (a2) or (a3).
