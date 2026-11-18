# Inputs repeating a chunk were refused by every holder, for good (2026-11-18, §4.16 "Apply on holders", A-383)

Found by reading `held_inputs` while building A-286 (version 0's inputs placed a slice at a time) for goal g232: one
primary and two replicas editing the Linux kernel tree. Recorded in GAPS then as a sibling owed a failing test first.

## Description

A holder recomputes every version of a green before it accepts the version's merge record (§4.16 "Apply on holders").
For a version after 0 it read the increment from its content hold with `held_inputs`
(`crates/server/src/merge_service.rs`): the inputs archive copied out of the hold (`ContentHold::archive_of`) and
restored with `slates_archive::restore`, admitted against a budget of the archive's chunks' raw bytes and one chunk's
decode space. The doc comment gave the rule: a well-formed inputs archive names each chunk once, so its file is never
longer, and a hostile one naming a chunk many times is refused before allocating (AUD-29-13).

The archive the hold returns carries each chunk once, by identity (`ContentHold::archive_of` walks
`referenced_chunks`, which dedups). An inputs archive is cut at fixed offsets (`merge_origin::InputsBuilder`, chunks of
`MAX_CHUNK_BYTES`, 1 MiB), so inputs that repeat a chunk's bytes at the cut name one chunk at several extents. A run of
one byte `R` chunks long covers at least `R - 1` whole chunks of the cut wherever it lies; three chunks are enough. The
file then restores to more than the distinct chunks' bytes and one more, `restore` refused it `OverBudget`,
`held_inputs` returned `None`, and the record path counted `merge.inputs_unheld` and answered nothing, though the holder
held the inputs whole. The owner shipped the record again every pass; every holder refused it every time.

## Measured

The failing test, `a_version_whose_inputs_repeat_a_chunk_is_recomputed_by_every_holder`
(`crates/server/tests/fleet.rs`): on a settled three-node fleet at `f = 1`, a green and a work over it; the work writes
one file of a run of `b'r'` three chunks long (3 MiB), a page a request (780 edits of about 4 KiB), and submits. It
asserts every edit declared, the submit answered `Submitted { version: Some(1) }`, both holders' replicas at version 1
and not refused, and each holder's held inputs naming one chunk at more than one extent (the case meant).

Kiln r49 (2026-11-18, the Mac Studio `d1323898`, debug build, a clone of `agents/g232-100-000-clients-repeated-chunk` at
2a626a78e; logs `~/.cache/g232-logs/repeat-2/` there):
- **Red** (the change's server code reverted in a copy, main's `held_inputs`): the submit ended `DeadlineExceeded` at
  the test's 60 s bound; both holders stayed at version 0 and counted `merge.inputs_unheld` 4,583 and 4,582 times; the
  owner counted the inputs shipped in parts (`merge.inputs.partial` 4) and never placed. Failed after 470.8 s.
- **Green**: accepted as version 1, both holders at version 1, both holders' inputs name a chunk twice. Passed in
  4.15 s.
- Unit: `a_holder_reads_inputs_that_repeat_a_chunk_and_charges_them_while_it_holds_them` and
  `inputs_naming_a_chunk_past_what_the_shard_admits_are_refused_before_any_is_gathered` passed, with
  `merge_origin`'s 11 tests and the record-identity test (14 of 14); both new ones failed with the charge taken as zero.
- The first run (r3646) failed before the case: the test named its work as its green, and a work and a green share one
  name space (`verbs::create_work` refuses `AlreadyExists`); fixed in the test (2a626a78e).

## Root cause

The cap stated a rule about the archive's layout (each chunk named once) that the archive's writer does not keep: a
fixed-size cut names a chunk wherever the inputs repeat its bytes. The restore admitted the file against what the hold
admitted, not against what the file is.

## Fix

`held_inputs` reads the inputs the way the replica's rebuild and seed already read them
(`crates/server/src/merge_origin.rs`): `PlannedInputs::of` plans the held manifest (its one file's extents and declared
size, the size no more than its extents can hold, `INPUTS_UNDECODABLE` otherwise), and `PlannedInputs::gather` gathers
it extent by extent from the hold (`Gather::planned`, `Gather::reserve`, `Gather::step`). Before a byte is reserved,
what the recomputation holds is charged to the shard's budget (`Store::grow`): the gathered bytes and the increment
decoded from them, whose post-state copies at most as many (`RECOMPUTE_COPIES`, two). The record path releases the
charge once it has recomputed, on every path. A shard that cannot admit it refuses `merge.inputs_over_budget`
(`INPUTS_OVER_BUDGET`); the owner ships the record again. A manifest naming one chunk at many extents is still refused
before anything is allocated, now at what the shard can admit.

## Impact

At `f = 1` with one primary and two replicas a version commits at the owner and one holder; with both holders refusing,
any version whose increment repeats a chunk's bytes at the cut (a large file of zeros or of one repeated pattern, written
or copied in one work) never committed, its submit waited out its deadline, and the record plane re-shipped it every
pass. Every later version of the green waited behind it.

## Siblings (swept)

- The rebuild and seed paths (`ReplicaSeed`, A-286 and A-313) and the takeover's advance (A-333) gather by extent and were
  never subject to the cap; their transient bytes are not charged to the shard's budget (GAPS, A-286's owed list).
- `slates_archive::restore`'s other caller, a taken-over plain volume (`verbs::materialize_taken_over`), admits against
  the volume's bound and what the shard can admit, not against distinct chunks: not this bug.
- `ContentHold::archive_of`'s other callers (`fleet::materialize`, `Daemon::fleet_held_archive`) restore nothing.
