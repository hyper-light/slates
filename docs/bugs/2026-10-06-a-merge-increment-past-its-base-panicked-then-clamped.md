# A merge increment whose ops ran past its base panicked the merge, and once bounded was clamped and accepted

Date: 2026-10-06.
Area: §4.16 merge engine, `crates/merge/src/engine.rs` (`apply`, `decide`).
Conditions: 3 (nothing reaches disk except an approved merge) and 11 (no panics).

## Description

Every `[]` index and slice in shipped code was swept to `get`. While sweeping, this replay line looked reachable:

```rust
out.extend_from_slice(&base[start..end]);
```

It is in the byte splice that replays an increment's ops over its base file. A test made it fail:

- The base file is the 10 bytes `0123456789`.
- An intervening insert lands at offset 0.
- The submitted increment holds two ops: `Delete at 0 len 20`, then `Insert at 30`.

The overlap with the intervening insert sends the path to the whole-file identity check, which replays the
increment's ops on the base. The delete moved the cursor to 20, past the base's 10 bytes. The insert then
sliced `base[20..10]` and the merge aborted with `slice index starts at 20 but ends at 10`.

With the slice bounded, the merge no longer panicked, but it accepted the increment and the file became
`XXZ`. The splice clamps whatever falls outside the base, so a document that described no real edit of the
file was merged as if it did.

## Root cause

An increment's ops document is decoded from bytes, and the decoder checks only the document's own framing:
magic, version, kinds, paths and lengths. Nothing checked each op against the file it edits.

The deriver only ever composes ops that fit their base (`derive::compose_content`):
- every op starts within the base;
- a delete or overwrite ends within it;
- a truncate ends at its end;
- an extend starts at its end;
- the bytes an op adds come from the post-state.

The engine assumed that shape instead of checking it.

## Impact

- **Before the sweep.** A malformed increment, from a faulty or hostile submitter on the merge plane, aborted
  the merge service; in release `panic = "abort"` takes the daemon down with it.
- **After the bounded slice alone.** Such an increment would have been merged with clamped content.
- No such increment is known to have been submitted; the daemon's own deriver cannot produce one.

## Edits

- `fits_base(ops, base_len, post_len)` states the deriver's shape as a check.
  - `Green::unfit_content` applies it to every content path, against the base the path edits:
    - a created path edits an empty base;
    - a renamed-into path edits its source.
  - A path that fails becomes a conflict window of class `TypeChanged`, the class a malformed special-file op
    already got. Nothing is applied.
- The splice and its helpers read through `get`, so a replay never panics even on input the check would
  refuse.

## Tests

- `an_increment_whose_ops_run_past_the_base_is_judged_not_panicked` (`crates/merge/tests/engine.rs`):
  - With the old slice it fails with the panic above.
  - With only the slice bounded it fails with `Accepted { version: 3 }` and content `XXZ`.
  - Now it passes: a conflict, and the file unchanged at `XX0123456789`.
- `every_composed_op_set_fits_its_base_and_merges_to_the_final_content` (`crates/merge/tests/splice.rs`)
  guards against false refusals:
  - Any generated journal over any base, composed by the real deriver and submitted with nothing
    intervening, is accepted, and the file equals the journal's final bytes.
  - Mutation check: making the extend rule `at < base_len` fails it at once (minimal input: a 58-byte base).
- Merge suites green: 10 + 14 + 41 + 44 + 5 + 7 + 9 + 9 + 9 tests. The server's merge and landing suites
  (`daemon`, `observe`, `landing_fairness`, `snapshot_landing`) are green: 20 + 3 + 3 + 8.

## Siblings

The other splice readers already clamped through `get` or now do: `post_slice`, `span_bytes`, `push_slice`,
and the per-range identity check. The fit check covers them all, because it runs before any of them on an
increment's own ops.
