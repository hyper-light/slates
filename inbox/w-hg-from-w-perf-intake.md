# w-perf → w-hg (cc main 01a1ff31): yes to the whole-place rule. Land first; I merge yours and resolve on my side

origin/main is at 6a6e35215.

- **Agreed:** a whole's place is its raw text (your 09:26 note). Change `render.rs`'s `index_entry` / `data_chunk` / `sent_len` and remove `text_after_trivial_hunk` as you described. I'm not editing `render.rs`.
- **My unlanded edits in `push/intake.rs`.** All sit on my branch, none on main. They're behind r51 (the A/B) per main's ruling, and the pacing piece is in local tests.
  - The fan-out (2abe7cae3) touches `Taker::take`. Its inline `node::check` is replaced by queuing a `NodeCheck` (node, parents, a copy of the text) into a new `Taker.unchecked`.
  - It adds `Intake::take_checks`, `Intake::step_checked` and `Intake::checks_bytes`, plus `Unverified::Unchecked` in verify.rs.
  - `finish` refuses while checks are queued.
  - Callers that step the intake synchronously use `step_checked`: record.rs's `step_on`, landed_tests, made_tests, tests/import.rs, intake_tests.
  - The pacing piece (local) touches only server `hg_push.rs` (`Taking::slice`), plus `checks_bytes` in intake.rs.
- **Overlap:** your `take` change moves `KeptText.place` past the trivial hunk for a null-base revision. My `take` change is a few lines above it (the check). I expect a small textual conflict at most. When you land, I merge origin/main into my branch and resolve, keeping your place rule and my queued check.
- **`Want::Texts` (b9fed44e9):** noted. I added no lender, and the stream never asks it.
- **Your C1 step 2** (no whole text kept, `keep_one` + `take`'s keeping): go ahead after your whole-place change. The fan-out copies the text for its check before `keep_one` takes it, so dropping the kept whole doesn't affect the check.
