# An export past the manifest depth bound emitted an unreadable archive

**Date:** 2026-09-30. **Area:** `slates-vfs` (export, §4.11), `slates-server` (refusal mapping).
**Found by:** AUD-29-13's sibling sweep
(`docs/bugs/2026-09-30-archive-decoding-and-restore-trusted-declared-sizes-and-layout.md`).

## Description

The manifest decoder refuses a tree whose path has more than `MAX_DEPTH` components (`PATH_MAX / 2`,
2,048). The VFS holds deeper trees, and the exporter walked them without a check. So a volume nested past
the bound was sealed into an archive that every holder's decode refused. Its placement and any takeover
failed far from the cause.

## Decision: keep the bound, refuse at the producer

The bound is a resource bound, not a host-path nicety:

- Restore keys each entry by its whole path, so a directory chain `d` deep costs `O(d²)` path bytes. That is
  the metadata amplification AUD-29-13 requires be refused.
- The server populates a takeover by path, at the same `O(d²)` cost.
- Raising the bound would also need iterative forms of `Node`'s encode, identity and compiler-generated
  drop.

So the producer respects the bound, as the reader does.

## Fix

- **`VfsError::TreeTooDeep { limit }`.** errno `ENAMETOOLONG`. The server answers `Unsupported { feature }`,
  naming the limit.
- **The export check.** `SnapshotArchiver::enter_directory` refuses a non-empty directory whose entries would
  pass the bound (frames, the root's included, equal the components of an entry inside).

## Test

`crates/vfs/tests/special.rs`, `an_export_deeper_than_the_manifest_bound_is_refused_typed`:

- a volume whose deepest path has exactly `MAX_DEPTH` components exports and decodes;
- one component more is refused `TreeTooDeep { limit: 2048 }` at the export.
