# A snapshot could not be presented through a mount (AUD-29-76, the snapshot half)

**Date:** 2026-10-01. **Audit:** AUD-29-76 (P2: "carry the authorized scope and version through the existing
attachment/view mechanism, including open-handle and advance lifetimes"). **Design:** §4.4 `attach(volume|snapshot,
...)`, §4.6.

## Description

A snapshot could be read only by an SDK (`ReadAt`). Every mount form refused it, and before this morning's fix the
host-mount form presented the head in its place. A pinned build input, or a sidecar sharing an immutable tree, had
no way to mount one.

## Exact edits

- `crates/server/src/snapshot_view.rs` (new): an attachment's read-only view of a snapshot.
  - It is a copy-on-write clone (`Volume::clone_of`, which pins the snapshot), kept beside the shard's volumes,
    keyed by attachment, so a publish never images it.
  - Its journal records are charged to the metadata ledger before it exists.
  - Closing it frees only its own records (`discard_partial`), unpins the snapshot and returns the credit, and
    counts any refused step.
  - On restart, `rebuild` makes the views again from the records; one that cannot be rebuilt ends its
    attachment, so its capability reaches nothing rather than the head.
  - A volume over a host base stays refused, since the snapshot does not hold the base's content.
- `crates/server/src/verbs.rs`:
  - a read host mount of a snapshot opens its view before the record commits (`commit_attachment`), and a write
    intent is refused;
  - `end_attachment` closes the view;
  - `destroy` closes the volume's views first;
  - recovery rebuilds views after the pins are reconciled.
- `crates/server/src/nfs.rs`: the edge serves a snapshot capability from its view, and admits it nowhere without
  one.
- `crates/server/src/oci.rs`: a container bind binds only to a mount that presents the version the bind asks for:
  the head, or exactly that snapshot. A bind of a snapshot mount is read-only.

## Proof

- `crates/server/tests/attach_forms.rs` `a_snapshot_host_mount_presents_the_snapshot_read_only_and_pins_it`:
  - the snapshot's mount reads `before` while the head holds `after!`;
  - a write through it is refused and a write intent is refused typed;
  - the snapshot is pinned while mounted and destroyable after the detach.
  - Mutated to serve the head, the test read `after!` and failed.
- `a_snapshot_host_mount_presents_the_snapshot_again_after_a_restart`: a second daemon over the same anchor segment
  serves the same capability with `before`. With the rebuild mutated out, the mount is refused (`MNT` 2) rather
  than presenting the head.
- `oci::tests::a_bind_binds_only_to_a_mount_presenting_the_version_it_asks_for` fails with the version match
  mutated out.

## Not done here (AUD-29-76's subtree half)

A subtree export. Its scope must be enforced by the export, not by a path check before the bind. The export's root
handle sits at the subtree, `..` there answers the subtree itself, and a handle naming an inode outside the scope
is refused. Otherwise a component swapped for a symbolic link after the check lets the runtime bind a host
directory.
