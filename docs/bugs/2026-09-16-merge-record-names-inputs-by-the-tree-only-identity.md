# A merge record names its inputs by the tree-only identity, so no holder ever finds them and every version past 0 waits `INPUTS_UNHELD` for good

Date: 2026-09-16
Area: `crates/server/src/merge_service.rs` (`enqueue_record`, the identity a merge record names its
inputs by) against `crates/cluster/src/content.rs` (the identity a content put binds and a hold keys by)
Severity: every merge version ≥ 1 of a green at `f > 0` never places — the owner reports it unplaced
forever, no holder recomputes it, `await placed` never answers placed. Surfaced as two fleet tests red on
both CI gates lanes (`a_holder_whose_recomputation_mismatches_refuses_the_version_loudly`,
`a_merge_record_is_issued_only_once_its_inputs_are_placed`; ubuntu-latest run 35131209643: 40 passed,
3 failed, 754 s) and failing at rest on the dev box in 496 s.

## Description

A green's version 0 (a scratch green: `inputs: None`) commits and replicates. Version 1's record names the
increment's inputs; the owner puts the inputs archive to the holder, the holder acknowledges it, the owner
ships the record — and the holder refuses it `merge.inputs_unheld`, every period, forever. Observed on a
clean build of HEAD `b2f1ef7` (a fresh target directory), one second after the submit and every second
after: `A.refusals = {merge.inputs_unplaced: 2}` (the put placed — the counter stopped),
`B.holder = {version: Some(0)}`, `B.refusals = {merge.inputs_unheld: 6, 14, 22, … 94}` (one refusal
per re-ship, about eight a second). No loud line: `INPUTS_UNHELD` is the quiet "the record waits" refusal.

## Root cause

`enqueue_record` (`merge_service.rs:965-969`) computed the record's inputs identity as
`inputs_archive(..).manifest.identity()` — the **tree's** identity. Since `69edd6d` (2026-09-15, "archive:
carry ownership … format minor 2") the root directory's own metadata is part of the manifest identity, and
the content plane keys everything by `Archive::manifest_identity()` (root metadata ‖ tree): the owner's put
binds it (`content.rs:744`), the holder's hold stores under it (`ContentHold::hold`, 514) and looks up by
it (`archive_of`, 541). The record therefore named an archive under an identity no hold ever has. That
commit fixed the same drift on the takeover path (`with_chunks` shipping a partial archive without the root's
metadata, recorded in its message) and missed this one: the merge inputs archive is built with a default
`root_meta` on both sides, so the put and the hold agree with each other and disagree with the record.

Bracketed by use: the parent of `69edd6d` passes the test in 6.0 s (own target directory, first build);
HEAD fails in 496 s. The last recorded pass of the test in this session's transcript is 2026-09-14 14:54Z,
before the commit.

## What was misdiagnosed, kept on record

Two earlier readings of this red were wrong and are corrected here. (1) It was attributed to scheduler
starvation retiring a live peer (the SWIM suspicion window); the failing runs show the coordinators ticking
every period for the whole 4000-period budget at rest, with the peer alive throughout. (2) The first two
cuts of the quantum work were blamed for "496 s at rest" — the same failure occurs on pristine HEAD; those
cuts are behaviourally identical at rest. A third reading, that the holder's observation path was failing
fast, misread daemon A's spawn count for B's: B's control shard admitted 38,627,707 observations for
38,627,680 asks — the harness observed a true non-convergence. The stage was named only by dumping both
daemons' whole refusal maps under the failing history on a clean build.

Build hygiene lesson: cargo hashes path sources relative to the workspace root and judges freshness by
mtime, so two extracted trees sharing one target directory reuse each other's compiled crates whenever the
later tree's files are older than the earlier fingerprints; two "HEAD" runs here actually ran the parent's
libraries. One tree per target directory, first build only.

## Fix

`MergeShardState::inputs_identity(bytes, page)` — `inputs_archive(bytes, 0, page).manifest_identity()` —
is the one production function that names a version's inputs, used by `enqueue_record`. The doc on
`inputs_archive` states which identity is the archive's.

## Verification

- `the_record_names_the_inputs_by_the_identity_the_hold_keys_by` (`merge_service.rs` tests): holds the
  inputs archive in a real `ContentHold`, requires the identity the record names to be the hold's key and
  `archive_of` to return the bytes; non-vacuous: the tree's own identity is not a key and finds nothing.
  Written against the pristine HEAD copy first, computing the identity as HEAD's `enqueue_record` did:
  FAILS there; passes with the fix.
- The two red fleet histories rerun on HEAD + this fix alone (recorded in the commit).

## Sibling sweep

- `.manifest.identity()` remains only in tests (`merge_service.rs` and `vfs/src/export.rs`), where it
  asserts the tree's determinism, not a cross-side key; the merge test now asserts the archive identity.
- The harness conflation that made the failing runs uninformative — an unobservable daemon reads as a zero
  counter (`refusal_count`: `Option::None → 0`), and `poll_until` charges the budget against
  `min(now) − min(start)` rather than each daemon's own delta — is a separate defect in the test-facing
  observation path, taken up in its own change (typed observation outcomes, admission receipts, one
  deadline, paced retries, per-daemon progress).
