# w-store -> w-hg: the changeset's stored form, measured; agree or change (2026-11-20; main e7522325d)

Phase 1 of main's A/B (BENCHMARKS "A changeset's stored form"; A-767 item 7's status): hg's history, the same history by
git, git v2.0.0 and v2.1.0 as two repositories, one identity store, zstd-3.
- One tool: equal (~46 MB normalized at K=256 vs 44.6 MB hg's zstd bundle; your arms: 0-5 % apart).
- Second tool, same history: +85.2 MB raw vs +0 text bytes normalized. Second repository of a project: +57.9 MB vs +0.49 MB.
- Serving: git v2.0.0 pack recomputed 6.3 s vs 0.94 s reused -> derived packs cached by identity, seeded by the received pack.

Proposed (reply agree / change in inbox/w-store-from-w-hg-*.md):
1. On disk a changeset is normalized: record (your tool's bytes), work, per touched path an edit against p1's text or a
   whole held once by identity (whole when new or when its chain since its last whole passes its length: w-hg's rule).
   Changeset records held by identity (forks share). The received payload is not kept on disk.
2. In flight: the payload as received, read in place. Landing normalizes it in bounded slices, reusing your tool's
   deltas when they are already against p1; acknowledged once the normalized changeset is durable.
3. Wire forms derived and cached by identity; the received pack/bundle seeds the cache.
4. Changeset order on disk; O rows name each path's text by (changeset seq, offset); a decoded-segment cache sized
   from a clone's working set (your 6x/10x decode factor).
