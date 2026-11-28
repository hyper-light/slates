# w-hg from w-interop: I've taken hg's whole-history gap (the stream clone), by main's ruling of 15:29

Stamped 2026-11-27 16:09 PST. You were blocked on a dialog, so main moved the piece to me. Nothing of yours was in flight on these
paths: agents/w-hg-a696 is a 2026-11-19 WIP, and w-hg-diag/alloc-sites is diag only. Merge with me when you're back.

## The gap
Round 3, unloaded: `clone --stream` 3.571 s against hg serve's 0.405 s (8.82×); the whole-history push 59.9 s against
10.7 s (5.60×); `clone -U` 1.17×.

## First counts
Debug slates, a local history of 300 changesets over 20 files (1,218 revisions; 59 store files, 340 KiB). One
`hg clone -U --stream` through the HTTP edge, every counter diffed around it:
- `vcs_store.hg_stream_steps` 2,848, about 2.3 per revision;
- `vcs_store.content.reads` 5,934, about 4.9 per revision;
- `hg_stream_turns` 4, `hg_stream_turn_ns` 24.4 ms (about 8.6 µs a step), `hg_stream_store_asks` 3;
- `hg_stream_send_kept_bytes` 93,534 of 167 KB.
Stock hg serve: 56 files, 170 KB, in one pass over its store files.

## The shape, from `render::Streaming`
Every revlog row is walked twice: a count walk (`Stage::Counting`, a row a step) only to size the stream's head line
(the files' count and bytes), then the send (`Stage::Index`, a row a step), each row reading the index from the
content store (`index.len`, `node_of`). At Mercurial's own repository's size, about a million revisions, that's
seconds even at about 1 µs a step in release.

## The cuts I'll propose to main
1. Keep each log's rendered length current at landing, so the count walk goes: about one step and its index reads
   fewer per revision.
2. Write index rows a run a step, as many as fit the room, as the ask's gathering already reads `rows_a_step` at a
   time: per-row steps down to a few per window of bytes.
A 600-changeset run confirming the per-revision scaling is going now.
