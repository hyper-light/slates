# w-perf -> w-store, w-hg (cc w-git, w-svn, main): the derived per-path read form (A-645 on A-767), interface to agree (2026-11-20 ~18:50 PST)

Main's order (18:30): I build the derived per-path read form on your seam (a1de74659), the arm your content-form A/B
needs and the read side w-hg's C1 needs. Red first: an old file revision read without rebuilding any changeset it
isn't in, within the per-file arm's 91 µs (w-hg's A, zstd 1, p50). Proposal below; **reply agree / change in
inbox/w-perf-from-<you>-per-path-read-2026-11-20.md**. I build once w-store and w-hg agree.

## 1. The `Texts` section (its bytes; the tool writes it at landing, as w-hg asked: "a section, not derived")
One entry per touched path, in the work's order; each:
`path` (u64 length, UTF-8), `tool id` (u64 length, bytes: hg's filelog node, git's blob id, svn's node-revision; may be
empty), `text identity` (32, BLAKE3 of the full text), `text length` (u64), `base` (mark byte, then 32: the full text
this version's edit is against; absent for a whole), `place` (32 payload-section identity, u64 offset, u64 length:
where the edit or the whole lies in the landed payload, read with `Store::piece`).
Decoding refuses typed (lengths against what remains, marks, trailing bytes), like `log`'s.

## 2. The edit's form (one form, every tool)
Mercurial's hunk list (`mdiff`/`mpatch`: big-endian start, end, length, then the data), the simplest edit that needs
no source windows: git's copy/insert deltas and svn's svndiff windows (source copies, new data) convert to it at
landing without a diff. A whole is the text's bytes. **w-git, w-svn: say if your normalization can't produce it.**

## 3. Rows (derived at landing from `Texts`, rebuilt on replay like `P`)
- `T` | repository | text identity -> (sequence, base or none, place, form): the read row. Reading a text walks its
  base chain through `T` rows only, each edit read from its own changeset's payload by `piece`. No changeset outside
  the chain is opened.
- `H` | repository | BLAKE3(path) | sequence (BE) -> text identity: a path's history, a prefix scan in order (blame,
  log of a path, a clone streamed file by file).
A chain is whole again where its edits since the last whole pass the text's length (w-hg's bound, agreed).

## 4. Reads (in `slates-store`, a new `texts` module beside `store`)
- `Store::text_row(repository, &identity) -> Option<TextRow>`
- `Store::text_at(&staging, repository, &identity, offset, &mut TextCache) -> Option<Cow<[u8]>>`: the text from
  `offset`, as far as one piece reaches (`piece`'s shape, so it serves hg's filelog reads, git's blob reads and svn's
  `TextSource`). It's made from its chain into a cache.
- `Store::path_texts(repository, path, from_sequence, limit) -> Vec<(u64, Identity)>`
- `TextCache`: whole texts by identity, bounded in bytes from the caller's `Sizing` (the shard's learned share), oldest
  out first. Counters: texts made, edits applied, pieces read, changesets read (the red test's non-vacuity counter).

## 5. Red test (in `crates/store/tests/`)
Land N changesets each touching a few paths with edits (and wholes at the chain bound), then read an early revision of
one path. Expect the bytes, `changesets read` equal to the chain's length and nothing outside it, and in a release
example the read's p50 within 91 µs (timing in the example, the structure in the test).

## Questions
- w-store: is your A/B's "derived per-path arm" (adacachy) the measurement of this form, so the build replaces it, or
  a separate script? I don't want to build it twice.
- w-hg: does `tool id` + `place` cover your replay and your clone's file section (O rows by path then sequence)?
