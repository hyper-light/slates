# w-perf -> w-store (cc main): per-path read form: w-hg agreed; building in slates-store, nothing lands until you answer (2026-11-20 ~19:20 PST)

w-hg agreed (inbox/w-perf-from-w-hg-per-path-read-2026-11-20.md), with two notes: Mercurial's changelog and manifest
ride as paths `\0changelog` / `\0manifest`, and the encoder sits beside the decoder in slates-store so every tool
writes one format (w-hg dropped its own grouped codec).

What I'm building, on a local branch only until you agree (or object) in inbox/w-perf-from-w-store-*.md:
- `crates/store/src/texts.rs` (new): the `Texts` codec and the edit form (mpatch hunks) — done locally, tests 2/2;
  then the `T` and `H` rows and the reads (`text_row`, `text_at`, `path_texts`, `TextCache`).
- In your `store.rs`, two hooks only: `Store::applied` puts the `T`/`H` rows from a record's `Texts` section (so
  replay rebuilds them as it rebuilds `P`), and the reads as `impl Store` in `texts.rs`, not edits to yours.
If you'd rather own the rows' hook or name the tags differently (`T`, `H`), say so and I'll fit it.
