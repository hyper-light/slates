# A work's second submit conflicted with its own committed bytes

Date: 2026-09-13. Area: `crates/server/src/verbs.rs` (`submit`), §4.16 "Submission".

## Description

After a work volume's increment was accepted (say as version N), a later edit on the same work
followed by a second `submit` was refused as a merge conflict — a `CreateCreate` window on the
file the first submit had itself created — instead of landing the later edit as version N+1.

Reproduced by `submission_barrier_scenario` in `crates/server/tests/daemon.rs` (folded into
`the_merge_service_enforces_roles_pins_versions_and_seals_behind_the_barrier`): create a work,
edit `f` to `hello`, submit (accepted, version 1), edit `f` again (append ` world`), submit. With
the fix disabled the second submit answered
`conflicts: [MergeWindow { path: "f", at: 0, len: 0, class: 4 }]` (class 4 = `CreateCreate`);
with it, `version: Some(2)` and `f` at version 2 reads `hello world`.

## Root cause

`submit` sealed the work's journal into an increment and, on acceptance, appended the merge
record — but left the work untouched: its `journal` still held every declared operation and its
`base_version` stayed at the old base. The next submit composed the *whole* journal (the already
merged operations plus the new ones) against the old base, so the increment re-declared the
file's creation with the now-longer bytes; the engine, seeing a create of a path it already holds
with different bytes, refused `CreateCreate`. The design says the seal consumes the declared
operations ("submit seals the work volume … composes its declared operations into a net op set";
a `stream` work "submits at every auto-seal"), which is only coherent if an accepted submit moves
the work's base to the new version and empties its journal.

## Impact

Any agent that submitted more than once from one work volume — the design's ordinary streaming
loop — was refused after its first accept, with a conflict window that named its own bytes. The
existing scenarios never re-submitted a work, so nothing caught it.

## Exact edit

`crates/server/src/verbs.rs`, `submit`, on `Outcome::Accepted { version }` after the durable
append: the work's `base_version` becomes `version`, its `journal` is cleared, and its `content`
becomes the green's files at the new head (the accepted work equals the green at its new base, so
a later edit declares against what the green holds). A conflict changes nothing, as before.

## Siblings

- `rebase` already moved the work's base, content and journal on a clean rebase, which is the
  same footing; `submit` was the one path that did not.
- Retention is unaffected: the accepted increment's bytes are in the `GreenAdvanced` record, so
  emptying the work's journal frees no input to the verdict (asserted by the same scenario: the
  work is destroyed and versions 1 and 2 still read back).
