# Work volumes grew uncharged

Date: 2026-10-05. Scope: work volumes of the merge loop (§4.16) and the shard budget (§4.2).

## Symptom

Found by reading `edit` while fixing the 4 KiB channel limit: a work's `edit` and `declare` grew its content map
and journal with no charge and no limit. A work was created `Dynamic { max: 0 }` with its content outside the
charged store, so an agent could fill a shard's memory through one work, and creating a work copied the green's
whole content for free. Greens were charged (their retention is settled against the shard budget, A-16); works were
not.

## Root cause

Works are kept in a `BTreeMap` beside the volume core, the design's stand-in for a VFS-backed work ("a mounted work
would keep this in its VFS tree; here `edit` maintains it directly"). That stand-in never joined the budget's
accounting, against banned item 8 (every structure has a derived bound and a typed refusal at it).

## Fix

- Failing test first: `a_works_edits_are_charged_and_refused_typed_when_the_budget_cannot_hold_them`
  (`crates/server/tests/daemon.rs`). Under the memory-pressure hold it expects an edit refused `BudgetExceeded` with
  the file unchanged, and a new work of a non-empty green refused. It fails with the charge disabled.
- `crates/server/src/work_charge.rs` charges a work for what it keeps: every file's path and bytes, and every
  journal operation's size and heap (`VolumeOp::footprint`, exhaustive over the variants). The charge uses
  dynamic-volume growth (`ShardBudget::grow`) and is taken before a verb changes anything: edits and declarations
  by their exact delta (`edit_delta`, `declare_delta`), a create, a rebase or a recovered work by a recount. A
  submit secures a bound (the green's files) before the verdict and settles it after, releasing it on a conflict
  or a failed append. Destroying, pruning or failing to rebuild a work releases or never takes it.
- Proven against a recount: `the_incremental_charge_equals_a_recount_after_every_step` generates histories of
  edits, unlinks and renames (onto existing files and onto themselves included) and compares the running charge
  with `footprint` after every step.

## Siblings

- A work still copies its green's whole content at creation (now charged). The copy-on-write share of the green's
  bytes is the measured optimization owed with VFS-backed works.
- The staging buffers of A-83 were charged from the start (the metadata budget).
