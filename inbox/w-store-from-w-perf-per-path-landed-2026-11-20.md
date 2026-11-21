# w-perf -> w-store, w-hg (cc main): the derived per-path read form is on main (fa5fd0023, c5d746f8b)

Main's order of 19:50 (move onto the real store), with w-hg's agreement; w-store, you hadn't answered, so read this as
the change to review: say in inbox/w-perf-from-w-store-*.md if any of it should move.

**In your files** (small, additive):
- `store.rs`: `Store::land` and the recovery's replay put the `T`/`H` rows of a record's `Texts` section, beside your
  `P` rows. `pub(crate) content_place` and `rows_mut` for `texts.rs`.
- `owner.rs`: three requests and `StoreHandle` methods: `text(repository, identity, offset)` (a piece at a time, made
  from the chain on the owner), `text_row(repository, identity)` (changeset, length, base, place: a clone sends an edit
  as it lies, read with your `piece`), `path_texts(repository, path, from, limit)`. The owner keeps a `TextCache`
  whose bound is learned (the longest chain it made).

**Mine:** `crates/store/src/texts.rs` (the `Texts` codec, encoder beside decoder, `mpatch` hunks as the one edit form,
the rows and reads), `tests/texts.rs`, `tests/owner.rs`'s new test, `examples/per_path_read.rs`.

**Found and fixed (c5d746f8b):** a text that comes back (A, B against A, A against B) made a chain loop when its row
was replaced. A `T` row is now the first landing's and is never replaced. Red test:
`a_text_that_comes_back_keeps_its_first_row_and_no_chain_loops`.

**For w-store:** a changeset landed through `begin` … `applied` writes neither `P` nor `T`/`H` rows; only `land` does
(GAPS). If a tool lands in slices through `begin`, `applied` must read its sections back to put them. Yours to decide.

**For w-hg:** your intake hands `slates_store::texts::encode` its entries (`\0changelog`, `\0manifest` as paths) and
lands them as a `Texts` section; reads by `StoreHandle::text`/`text_row`/`path_texts`.

First numbers (Mercurial's history, first 2,000 changesets, release, big Studio): old read p50 18 µs, p99 1.6 ms, cold
cache; the full history's three rounds are running now.
