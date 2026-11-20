# A sealed-block writer dropped with a reader still registered kept its blocks (2026-11-18)

## Description

Miri's run of the memory crate's unit tests (`cargo +nightly miri test -p slates-mem --lib sealed`, adacachy run r241
on main `3022c6894`) passed five tests and then failed the run: "memory leaked: alloc232238 (Rust heap, size: 5,
align: 1)", allocated by `to_vec` in `sealed::tests::a_released_block_stays_until_every_reader_passes_its_release`. That
is the test's `fresh` block, sealed into a writer whose reader had joined and never left, and still live when the test
ended. CI's Miri lane runs `-p slates-mem --lib`, so main's lane went red.

## Root cause

`SealedBlocks`'s drop freed nothing while a reader was registered, and it was right not to: a registered reader may be
on another thread, still reading any block it holds, and nothing in the store can know that it never will. Freeing
would hand it freed bytes. Only shared ownership could tell when the last of the writer and its readers is gone, and
D-8 rules it out. The defect was that the rule this leaves (a reader's lifetime is bounded by its writer's, so the
writer's holder makes every reader leave before it gives the writer up) was nowhere written as the rule, had no API a
holder could use to give a writer up safely, and was broken silently: the test dropped its reader without leaving, then
dropped its writer, which kept its one live block. A second test left a reader registered too, over a writer with no
block, which kept nothing and went unreported.

## Impact

The tests' memory, and a red Miri lane on main. No production path made a replica before A-273's step 5, so no owner
was dropped with a reader registered outside the tests. Had one been, the same silent keep would have held the
owner's content for the process's life.

## The rule, and the edits

The rule (`crates/mem/src/sealed.rs`, module docs, "Lifetimes"): a reader reads a block only while its writer keeps
it, so a reader's lifetime is bounded by its writer's. The writer's holder ends every reader (each leaves, giving back
its `Left`) before it gives the writer up, with `SealedBlocks::end`. A writer dropped with a reader still registered
frees nothing, since that reader may still read; such a drop is its holder's error, counted process-wide.

- `SealedBlocks::end(self) -> Result<(), Box<SealedBlocks>>`: frees every block, live and kept alike, once no reader is
  registered; refused while one is, the writer handed back unchanged for its holder to keep until its readers have left.
- `Drop for SealedBlocks`: with a reader registered, counts the drop (`sealed::dropped_with_readers`) and frees
  nothing; otherwise frees every block, as before.
- Tests: `a_released_block_stays_until_every_reader_passes_its_release` makes its reader leave and ends its writer;
  `changes_out_of_order_or_of_another_writer_are_refused` makes its reader leave; new tests
  `a_writer_ends_only_once_its_readers_have_left_and_then_frees_every_block` (refused while the reader is registered,
  every block freed once it left; a leak fails it under Miri) and `a_writer_dropped_with_a_reader_registered_is_counted`
  (a writer that holds no block, so the counted drop keeps nothing). The two tests the cross-lint lanes found over the
  cognitive-complexity threshold (main CI run 44397328424) are split with judging helpers (`writer_reads`,
  `reader_reads`, `writer_holds`): `a_block_reads_back_as_sealed_an_empty_one_too` and
  `a_release_with_no_reader_frees_at_once_and_its_slot_is_reused` replace the one test of both.
- Owed, A-273 step 5: the owner's holder on the shards ends a green's replicas (each leaves) before it gives the owner
  up, and its tests hold `dropped_with_readers` at zero.

## Siblings swept

Every other test of the module makes each of its readers leave before its writer drops (the cross-thread test, the
late-reader test, the generated histories). `slates-merge`'s tests of step 3 make every replica leave
(`Green::leave_replica`) before their owner drops.
