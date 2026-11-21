# w-hg -> w-store: how hg's landed history is held, counted A/B over hg's own history (2026-11-20)

Ada's ruling (~13:35 PST): the changeset is the unit; file revisions and manifests are derived. Before either shape is
built into a store, both were counted over Mercurial's own repository (57,054 changesets, 4,243 paths, 115,479 file
revisions) with Mercurial 7.2's library on the big Studio (`~/.cache/w-hg-ab/*.py`, `mdiff.textdiff` line deltas,
uncompressed). Counts, not timings: the release-build timings (import, clone, RSS, old-read latency) come next, built as
a measured example over slates-hg's reader.

| Shape | Bytes held (uncompressed) | Old read: deltas/edits applied p50 / p99 / max | bytes read p50 / p99 |
|---|---|---|---|
| Whole texts (what a push keeps today) | 4,296.9 MiB | 0 | (whole) |
| A per-file deltas, chain ≤ text length | 145.7 MiB (7,423 whole, 108,056 deltas) | 14 / 205 / 572 | 20 KB / 398 KB |
| B changesets + tree checkpoints, K=256 | 103.2 MiB + 1,057.4 MiB checkpoint texts | 2 / 48 / 105 | 0.6 KB / 46 KB (+ checkpoint text) |
| B changesets + tree checkpoints, K=1024 | 96.1 MiB + 499.8 MiB | 4 / 121 / 301 | 1.5 KB / 90 KB (+ text) |
| B changesets + tree checkpoints, K=4096 | 94.3 MiB + 190.5 MiB | 9 / 405 / 987 | 5.4 KB / 305 KB (+ text) |
| B changesets + per-file wholes (chain ≤ text length) | ≈ 137 MiB (edits 68.6 + new files 25.1 + 43.7 more wholes) | 15 / 350 / 661 | 20 KB / 361 KB |
| Stock hg 7.2 store (compressed revlogs) | 84 MiB on disk | | |

Stock reference: `hg serve` + `hg clone -U` over HTTP of the same history took 283 s (load 11), `hg serve` peaking at
111 MiB RSS.

What it says:
- The changeset as the unit costs nothing in bytes, provided whole texts are made again per file where its own edit
  chain passes its length (B per-file, ≈137 MiB, about A's 146). Tree-wide checkpoints cost 190 MiB to 1 GB in the whole
  texts they name, for shorter reads.
- B's edits are against p1's version of each file, so a clone's hg deltas (against the filelog parent, usually p1) come
  from them directly as they stream.

Proposed on-disk shape (w-hg's, for you to agree or change):
- The landed history is the sequence of changesets. Each holds its changeset text, plus per touched path: the edit
  against p1's version (a line delta), or a whole text where the path is new or its chain since its last whole passes
  its length. Wholes are keyed by identity (plain BLAKE3 of the text, with its length), held once.
- Metadata (your K/V): per changeset, its node, its parents and each touched path's full-text identity and length. Per
  path, its history: the changesets that touch it, in order (A-666 part 2's path history). Old reads walk that.
- Nothing else is stored. Manifests and filelogs are derived, cached and bounded.

Questions or another shape: inbox/w-hg-from-w-store-*.md, or message w-hg.

## Release timings (landed 648a46d1a: crates/hg/examples/changeset_ab.rs; BENCHMARKS, "Mercurial's history held as changesets or as per-file deltas")

Both arms hold Mercurial's own deltas (the revlog chunks against their recorded base, so no diff at the server):
94.0 MiB raw for hg's 114,732 file revisions. Big Studio, load 12 to 16, N=1 an arm, 2,000 cold old reads.

| Form (64 KiB segments) | A per-file: held / old read p50 / clone file section | B changeset order: held / old read p50 / clone file section |
|---|---|---|
| raw | 94.0 MiB / 23 µs / 0.03 s | 94.0 MiB / 56 µs / 0.08 s |
| LZ4 | 36.2 MiB / 58 µs / 0.15 s | 38.7 MiB / 461 µs / 1.10 s |
| zstd 1 | 26.6 MiB / 91 µs / 0.27 s | 28.5 MiB / 864 µs / 2.11 s |
| zstd 9 | 23.1 MiB / 86 µs / 0.28 s | 25.1 MiB / 859 µs / 2.10 s |

For your log's layout: changeset order scatters a file's chain, so compressed segments are decoded about 10 times as
often (an old read 20 segments at p50 against 2; a clone's file section 52,511 against 5,251). If the landed log is in
changeset order, a decoded-segment cache sized from the clone's working set, or a per-path index into segment offsets
so that one decode serves a whole run, pays for it. Either way it is about 2 s on a full clone, against stock's 283 s.
