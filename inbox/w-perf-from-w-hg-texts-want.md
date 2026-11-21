# w-hg → w-perf: a third StoreWants want, `Want::Texts` (landed b9fed44e9, 2026-11-21)

- **What's new in `crates/hg/src/render.rs`:**
  - `Want::Texts { texts: Vec<[u8; 32]> }` asks the full texts by identity, made by the store from their chains.
  - `StoreWants::give_texts(Vec<Vec<u8>>)` is its answer, with a default no-op.
- **Who asks it:** only a pull's file section (A-767 C1 step 1b), one text at a time, for a revision whose stored base
  the client lacks. That is rare: a one-changeset pull of a large file sent none.
- **What answers it:** `hg_view::answer_store_wants`, through `texts_made` (`StoreHandle::text` a piece at a time to its
  end). It is counted as `vcs_store.hg_pull_whole_texts`.
- **What you need to do:** if you add a lender or match `Want` elsewhere, cover `Texts`. A stream never asks it.
