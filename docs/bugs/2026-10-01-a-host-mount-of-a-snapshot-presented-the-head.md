# A host mount of a snapshot presented the live head (AUD-29-76)

**Date:** 2026-10-01. **Audit:** AUD-29-76 (P2: "do not substitute current head for a snapshot"). **Design:**
§4.4 `attach(volume|snapshot, ...)`, §4.6.

## Description

`attach` accepted a snapshot with the host-mount form and returned a mount capability. The NFS edge admits
every capability as `View::Current`, so the mount presented the volume's live head where the snapshot was
asked for. Measured by the test before the fix: the snapshot held `before`, and its mount read `after!`. The
FUSE and OCI forms already refused a snapshot (`SnapshotNotPresentedByHostMount`); only the host-mount form
did not.

## Root cause

`verbs::establish_form` returned early for the two record forms, `Root` and `HostMount`, before any snapshot
check. That is correct for `Root`, whose reads name their own version. It is not correct for a mount, which
presents whatever the edge serves.

## Exact edits

- `crates/server/src/verbs.rs`: `HostMount` with a snapshot is refused
  `AttachmentUnsupported{NfsLoopback, SnapshotNotPresentedByHostMount}` before anything is recorded.
- `crates/server/src/nfs.rs` (`admit_mount`): a capability whose record names a snapshot is not admitted.
  Such a record could be one recorded before this fix and recovered from the log; the edge never serves the
  head for it.

## Proof

`crates/server/tests/attach_forms.rs` `a_snapshot_is_never_presented_through_a_host_mount_of_the_head`: write
`before`, snapshot, write `after!`, attach the snapshot as a host mount. Before the fix it failed with "a host
mount of the snapshot was attached and presents \"after!\"". After the fix the attach is refused typed, with
the attachment count unchanged. attach_forms passes 5/5.

## Sibling sweep

- `Root`: an SDK record; its reads name their version (`ReadAt`), so no view is substituted.
- FUSE and OCI: already refused a snapshot.
- Guest admission: carries no snapshot, so it cannot present one wrongly.

## Not done here (AUD-29-76 remains open)

Snapshot and subtree exports, carrying the authorized version and scope through the attachment's view with
their open-handle and advance lifetimes. Until then both stay precise refusals: a snapshot by the reason
above, and a directory inside the mount by `NotAMountPoint`.
