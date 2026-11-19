# Every checkpoint carried the identity's sum, so a small green folded late (2026-11-19, a regression A-626 part 3 made, found by g258, bisected by main; fixed in A-626 part 3, continued (1), g307)

## Description

slates-server's `repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log` writes a
repository's root green twice, each through a work destroyed after, so no reader holds the root's first version, and
expects a branch at version 1 to be refused `UnknownBase`: the root folded that version. From A-626 part 3 (637a65d69)
on, the branch was made (`GreenCreated`), and the test panicked at `repository.rs` 745 and 749 (`daemon.rs` 7112, the
audit fixture's own `expect`). It passed at a6306963e, main's commit before part 3 merged; main's bisect (kiln r104,
Mac Studio) put the change at 637a65d69.

## Root cause

A branch at a version is refused below the oldest version the source reconstructs, in memory or at a recovery
(`repository::branch_version`: the engine's `folded_below`, or the partition's durable floor, `green_floor`). A small
green never folds in memory (`fold_history_to_budget` folds only as far as its retention budget needs), so the root's
version 1 is refused only once the durable chain is checkpointed at 2 or later.

The service checkpoints a green's chain by the doubling rule (`merge_service::checkpoint_green_when`): a checkpoint is
taken at the floor once the increments it would fold reach the size of the checkpoint it holds, so taking checkpoints
costs at most the bytes appended. A-626 part 3 made the checkpoint version 4, which wrote the identity's running sum,
LtHash16's 1,024 lanes, 2,048 bytes, into every checkpoint, though a restore needs it only beside a copy's record:
without one, the sum holds nothing beyond the restored state's own entries, which the restore hashes as it places them.
So every checkpoint grew by 2,048 bytes, and the rule waited on 2,048 bytes of increments for every green. Measured
with the rule's inputs printed (kiln r122, Mac Studio, main 3329211, 2026-11-19): the first write's checkpoint at
version 1 was taken at 2,233 bytes; the second write would have folded 194 bytes, under 2,233, so no checkpoint was
taken at 2, the durable floor stayed at 1, and the branch at 1 was made. By the layout, the same checkpoint less the sum
is 185 bytes, and version 3's was 153 (version 4 added the 2,048 bytes and 32 of empty copies' tables).

## Impact

Every green, copied or not, paid 2,048 bytes in each checkpoint in its partition's log and snapshots, and computed the
sum at each checkpoint (`Green::sum_at`, what a branch at the version costs), for nothing it ever read. And the
doubling rule held up to about 2,048 bytes of foldable increments in each green's durable chain, so a green that
accepts small increments kept versions no reader held, and recovery replayed them. Nothing was wrong: the versions kept
are reconstructible and every answer was right; the cost was the waste and the fold's lag. The one test that met it
asserts the fold of a version no reader held.

## Edits

- `crates/merge/src/checkpoint.rs`: version 5. The copies' section (the records and the copied tables) and the sum each
  sit behind a presence byte (as the head check does), written beside a copy's record alone: `Checkpoint::sum` is
  `Option<Sum>`, `encode_copies_into` writes the section, `take_copies_section` and `take_sum` read them, and a
  presence byte that is neither is refused `BadKind`. A checkpoint of a green that never copied is version 3's tables
  and three presence bytes (no copies' section, no sum, no head check): 155 bytes for the test's root at version 1.
- `crates/merge/src/engine.rs`: `Green::checkpoint` takes the sum only where a copy's record stands at or before the
  version (`(!copies.is_empty()).then(|| self.sum_at(at))`), and the restore takes one only where the checkpoint
  carries it; otherwise the identity is the one the restore's placed entries make.
- `crates/merge/src/engine/copy_checkpoint.rs`: the module docs' cost line.
- `crates/anchor/src/layout.rs`: `LAYOUT_VERSION` 13, so a segment a build of layout 12 wrote (version 4 checkpoints)
  is refused at adoption, typed, before anything is served, rather than fencing a green mid-service at its first
  checkpoint.

## Tests

- The red: the repository test, failing on main (main's r104; r122 above, with the rule's inputs printed).
- `crates/merge/src/checkpoint/tests.rs`: the golden checkpoint holding a copy re-derived for version 5 (2,499 bytes,
  identity `05c1e7f6…3865f09`), and a new golden one of a green that never copied (115 bytes, identity
  `dfa590d1…6de08c04`), both by the pure-Python BLAKE3 of A-626 part 3, which still reproduces version 4's pinned
  vector; each altered presence byte of the latter refused `BadKind`; a version 4 header refused `BadVersion`.
- `crates/merge/src/engine/copy_tests.rs`: the replica test of A-626 part 3 compared the replica's and its owner's sums
  in their checkpoints at 3, below the copy, which carry none now; it compares the identity each moves to 3
  (`Green::sum_at`, what a branch there carries) and their whole checkpoints there.
- `crates/merge/tests/checkpoint.rs`: the hostile test's arithmetic (three presence bytes after the changed tables).
- `crates/anchor/tests/anchor.rs`: a segment of layout 12 refused at start.

## Evidence

macOS arm64, Mac Studio, debug, 2026-11-19; each run a fresh clone of the pushed branch, `CARGO_BUILD_JOBS=2`.

- Red: main's bisect (kiln r104) and r122 above, at main 3329211 with the doubling rule's inputs printed into the
  clone alone: the checkpoint at 1 taken at 2,233 bytes, then 194 bytes to fold against it, and no checkpoint at 2.
- Green, kiln r146 at 189ab8dc3 over main 545181b30 (the code landed here):
  `cargo test --locked -p slates-server --lib repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log`
  1 passed; run again with the rule's inputs printed, the checkpoint at 1 is taken at 155 bytes and the one at 2 at
  195, the second write's 194 bytes having outweighed the first. `cargo test --locked -p slates-merge --lib -- copy
  checkpoint engine::branch engine::node_tests engine::reanchor engine::stack::tests` 76 passed, none failed, the copy
  oracle's fixed history meeting all 13 census cases (its whole run: 279 checkpoints carrying copies, 275 listing copied
  keys, each restored and replayed to its owner's identity). `cargo test --locked -p slates-merge --test checkpoint
  checkpoint` 4 of 4; `--test chain_replay checkpoint` 3 of 3. `cargo test --locked -p slates-anchor --lib layout`
  2 of 2; `--test anchor a_segment_` 6 of 6, the layout 12 refusal among them. `cargo fmt -p slates-merge -- --check`
  and `cargo fmt -p slates-anchor -- --check` clean.
- The same runs over main b13a6e05f, after A-676 and the ipc protocol's and the server's submit changes (kiln r164,
  at 8919290ed): the repository test 1 passed; slates-merge lib 77 of 77 filtered; `--test checkpoint checkpoint` 4 of
  4; `--test chain_replay checkpoint` 3 of 3; slates-anchor `layout` 2 of 2 and `a_segment_` 6 of 6; fmt clean.
- Owed (main's): the same runs on Windows x86_64 and Linux, and the red of the new tests by mutation.

## Siblings

- The repository test asserts a fold the doubling rule takes by bytes: it holds while the second write's increment
  outweighs the checkpoint at 1 (194 against 155 bytes now, 39 to spare; 41 before part 3, by the layout). A test that
  needs a version folded could ask for the fold rather than lean on the sizes; reported to main, not changed.
  Changed since (2026-11-19, g307, UI goal 93, on main's ruling): the test writes its root until the rule takes a
  checkpoint past version 1 (`repository::tests::write_until_folded`), reading the rule's own inputs after each write
  through `merge_service::fold_inputs`, which `checkpoint_green_when` now reads them through too: while no checkpoint
  is taken the rule must be waiting on them (the bytes to fold under the held checkpoint's size) and each write must
  add bytes to fold, so the writes end within that size. A checkpoint or an increment that grows or shrinks now moves
  how many writes the fold takes, never the test's verdict. A sweep of slates-server's unit and integration tests and
  slates-client's for any other fold awaited past a held checkpoint found none: the rest hold a reader below the head,
  wait only for a green's first checkpoint (taken at once, nothing held), or fold over hundreds of 8 KiB versions.
