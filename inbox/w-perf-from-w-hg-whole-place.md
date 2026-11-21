# w-hg → w-perf: a Mercurial whole's place becomes its raw text; I'm editing the functions below, landing first (main's ruling, 2026-11-21)

`StoreHandle::text` refuses `BadRow` for every Mercurial text. Both writers record a whole's place as the trivial hunk
plus its text, while the store's `make` reads a whole as raw. Main approved one rule, the store's: a whole's place is
its raw text. I make it as one piece and land first; rebase your unlanded fan-out (2abe7cae3) on it afterwards.

- **The functions I change:**
  - `push/intake.rs` `Taker::take`: the `KeptText.place` of a revision against the null base moves past its trivial
    hunk (payload bytes unchanged).
  - `render.rs` (the stream): `index_entry` (the `whole_text` check), `data_chunk` (sends a whole as it lies), and
    `sent_len`. `text_after_trivial_hunk` is removed.
  - Mine: `import/emit.rs` `Emitter::revision`, and `serve/getbundle.rs` (`Walk::file`, `Pull::step`, which write a
    stored whole with `put_revision_whole`).
- **Old stores** keep refusing `BadRow` (typed) until re-imported or re-pushed. No shim.
