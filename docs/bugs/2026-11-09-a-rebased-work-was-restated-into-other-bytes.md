# A rebased work was restated into other bytes (2026-11-09)

## Description

A rebase returns the work's journal restated in head coordinates (§4.16 "Rebase, the only corrective path"), and the
work's next submit composes that journal. Two kinds of journal came back saying something other than what the work
had said. The submit after the rebase was accepted, and the green took bytes the work never held.

- **A rename over a file.** A work renamed `a` over `b` and was rebased, even onto the head it was already at. When
  submitted, `b` read `a`'s bytes followed by `b`'s own, or by `a`'s again: 20 bytes where 10 were renamed in. Found by
  W1's generated oracle (`works_off_owner.rs`, perf-w1) after the merge of main 84d1f59a. On one shard, `g3` was "20 eeec…".
  On eight shards it was "20 c48c…": both wrong, and differently.
- **An edit that replaces bytes.** A work deleted the first byte of a one-byte file `a` and inserted three, then was
  rebased onto its own head and submitted. `a` read `[0, 0, 48]` where the work held `[0, 0, 0]`. The work's own byte
  survived and an inserted one was deleted. Found by this change's rebase oracle in its first case.

## Root cause

`Green::rebased_from` (`crates/merge/src/engine.rs`) turned the verdict's effects into the restated journal.

- **The rename.** The verdict states a rename into `dst` as `SetContent(dst, …, [insert_whole], Copied(from))` followed
  by `RemoveContent(from)`. Here, `insert_whole` stands for bytes the move supplies. The restatement wrote it as
  `Insert { dst, 0, len }`, plus a `Create` only when the head lacked `dst`, and then `Unlink { from }`. Composed over a
  head that holds `dst`, the insert adds the moved bytes in front of the destination's own. When `a`'s bytes equalled
  `b`'s, `changed` dropped `b` (no different from the head), so the bytes were read twice from the head's `b`.
- **The replacing edit.** A file's effect carries its net ops: one pass over the head, each op at its head offset, in
  order (`apply`'s cursor). The restatement wrote each op as a journal operation at that same offset. A journal is
  replayed one operation at a time over the file as the operations before it left it, so a later op's offset no longer
  named the byte it meant. `Insert@0` then `Delete@0, 1` deleted the first inserted byte, not the head's byte.

## Exact edits

- `crates/merge/src/engine.rs` `rebased_from`: an effect copied from `from` is restated as `Rename { from, to }`, as the
  composer states a base file renamed (`increment.rs`'s rules: "one `Rename` that replaces the target"). The source's
  removal that follows is the rename's own and is not restated as an `Unlink`.
- `content_volume_ops` (was `content_volume_op`) restates a file's ops where `apply` stands when it reaches each one.
  That position is past the output so far, with the head left from its cursor still after it. Each op keeps its kind.
  A truncate cuts the file where the output stands.

## Tests

`crates/server/tests/green_post_state.rs`:

- `a_rename_over_a_file_rebased_then_submitted_leaves_the_renamed_bytes`: red, `b` read 20 bytes. Passes.
- `a_rename_over_a_different_file_rebased_then_submitted_replaces_it`: red, `b` read 26 bytes (`a`'s ten, then `b`'s
  sixteen). Passes.
- `a_rebased_work_submits_as_it_would_have` (the oracle):
  - Two greens are built alike. On each, a work runs the same generated journal: edits, renames, unlinks, modes,
    symlinks, directories, xattrs, hard links and FIFOs. A second generated work is submitted ahead of it.
  - One green's work is submitted. The other's is rebased onto the head, then submitted.
  - Expect the same answer (a conflict as the same windows, a refusal with the same reason), the same root entries,
    and every file reading the same.
  - 400 cases pass. Of those, 294 merged after the rebase, every step kind among them (renames 47 times), 58 compared
    conflicts, 38 compared refusals, and in 10 the work ahead was itself refused.
  - Red-checked twice. Restating ops at their head offsets fails it (the minimum is two edits of one file). Restoring
    the rename's old restatement fails it (the minimum is one rename).
- `an_edit_after_a_rename_over_a_file_is_merged_with_it` is parked `#[ignore]`: the sibling below, open.
- Unchanged and green: the merge crate (175), `daemon` (25), `merge_load` (5).

## Siblings

- **Open: an edit after a rename is accepted and lost.** Found by the same oracle, and reproduced on main without any
  rebase. A work renames `a` over `b`, writes `XY` at the end of `b`, and submits. The submit is accepted, and `b` reads
  `a`'s head bytes alone.
  - `decide_content` skips the content ops of a renamed-into path ("handled with that effect"). `merge_rename` copies
    the source's head bytes.
  - The chain entry's `Applied::Copied { dst, from }` has no ops, so a replay could not reproduce the edit either.
    Replay maps all of an entry's effects against the state before the commit, so a second `Spliced` effect on `dst`
    would read the wrong bytes.
  - The fix needs a new chain-entry form (`Copied` with the destination's ops mapped through the source's deltas), a
    durable format change, and is owed to its owner. GAPS has the item.
  - The oracle's generator stays out of this case: it renames only base files the work has not touched, and makes no
    content ops on a path after renaming into it.
- The other restatements in `rebased_from` (specials, symlinks, links, directories, modes, xattrs) are covered by the
  oracle's generator and agree.
