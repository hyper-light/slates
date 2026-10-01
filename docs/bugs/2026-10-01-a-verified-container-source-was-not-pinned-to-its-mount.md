# A verified container source was not pinned to its mount (AUD-29-66)

**Date:** 2026-10-01. **Audit:** AUD-29-66 (P1). **Design:** §4.6 A-9 (the OCI handoff), `docs/wip/oci-handoff.md`.

## Description

`attach` in the container form verified that a host path was a slates mount of the volume — by the mount table's
path, type and source strings — and returned a recipe naming the path. Nothing tied the recipe to the mount instance
it verified: by the time a harness's runtime bound the path, the mount could be gone (a runtime given `-v` creates a
host directory there) or replaced by another mount instance at the same path.

## Root cause

The mount table kept no identity of a mount instance, so a later reader could not tell the verified mount from a
replacement.

## Exact edits

- `crates/bridge-oci/src/mount_table.rs`: `MountIdentity` on every `MountEntry` — Linux `mountinfo`'s mount ID and
  `major:minor`, macOS `statfs`'s `f_fsid` (read inside the existing `getfsstat` block).
- `crates/bridge-oci/src/verify.rs`: `VerifiedHostMount::identity`; `SourceChange` and `source_unchanged` (pure).
- `crates/ipc/src/protocol.rs`: `HostMountEvidence::mount_id`, `mount_device` (appended).
- `crates/server/src/oci.rs`: the binding carries the verified identity.
- `crates/mcp/src/lib.rs`, `crates/cli/src/verbs.rs`: the evidence JSON and text carry it; `slates oci-check`
  (`crates/cli/src/args.rs`, `main.rs`) reads the table and refuses `SourceMissing`/`SourceReplaced` (exit 1).
- `crates/cli/tests/cli.rs`: the container leg checks before each bind.

## Proof

The identity parse from proc(5)'s example line; the decision over unchanged, missing, remounted and shadowed tables;
a live macOS mount whose source checks clean and then refuses `SourceMissing` once unmounted; T-4.13 through Docker
Desktop with the check before both binds.

## Carried

The instant between the check and the runtime resolving the path stays: Docker resolves bind sources by path on its
daemon host, so no descriptor handoff can span it.
