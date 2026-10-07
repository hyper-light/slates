# `.` and `..` were `ENOENT` over NFS, and LOOKUPP never found a parent

Date: 2026-10-06.
Area: `crates/bridge-core/src/volume_bridge.rs` (`lookup`), `crates/bridge-core/src/scoped.rs`,
`crates/bridge-nfs/src/v4/compound.rs` (LOOKUPP).
Condition: 4, found by its hostile-names battery.

## Description

NFSv3 `LOOKUP "."` and `LOOKUP ".."` answered `NFS3ERR_NOENT` at the export root and in every directory. NFSv4
`LOOKUPP`, implemented as a lookup of `..`, answered `NFS4ERR_NOENT` everywhere.

Linux's client asks for a directory's parent when it holds only the directory's handle: `nfs_get_parent` (v3
`LOOKUP ".."`, v4 `LOOKUPP`), after a server restart reconnects a dentry, or for `open_by_handle_at`. Such a client
found no parent. knfsd answers both.

## Root cause

The volume core resolves child names only. `readdir` synthesized `.` and `..` in the shared bridge, but `lookup`
passed them to the core, which has no entry by those names.

## Edits

- `VolumeBridge::lookup` resolves `.` to the directory and `..` to its parent, the root's being the root. A
  non-directory is `ENOTDIR`.
- `ScopedBridge::lookup`: `..` at the scope's root is the scope itself, never the directory above.
- v4 `LOOKUPP`: when the parent found is the directory itself (the root of the namespace or of a scoped view), the
  answer is `NFS4ERR_NOENT`, as RFC 8881 §18.14.3 specifies. v3 `LOOKUP ".."` at the export root answers the root, as
  knfsd does.

## Tests

- `hostile_names_never_reach_outside_the_export_or_leave_an_entry` (`crates/bridge-nfs/tests/procedures.rs`):
  - `.` and `..` at the root and `..` in `d`;
  - LOOKUP and CREATE of `../../etc/passwd`, `a/b`, `/`, `d/..`, names with NUL, the empty name, non-UTF-8, and
    4096 bytes, all refused;
  - the root still lists exactly its two entries.
- `dot_and_dotdot_resolve_and_never_climb_out_of_a_root_or_a_scope` (bridge-core): whole and scoped.
- `lookupp_names_the_parent_and_is_noent_at_the_root` (`crates/bridge-nfs/tests/v4.rs`).
- Mutation check: without the resolution, both new tests fail.
- The hostile names were all refused before and after: no escape was found, only the parent gap.
