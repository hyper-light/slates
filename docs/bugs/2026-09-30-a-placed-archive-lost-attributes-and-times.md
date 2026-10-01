# A placed archive lost extended attributes and access and birth times

**Date:** 2026-09-30. **Area:** `slates-archive` (manifest, restore), `slates-vfs` (export, `set_times`),
`slates-cluster` (referenced chunks), `slates-server` (takeover rebuild). **Audit:** AUD-29-56 (P1).
**Design:** §4.5 "Extended attributes", §4.10 takeover, §4.11 / D-17 archive format, R8.

## Description

A volume's snapshot was archived, placed and verified, and a takeover successor then served a different
volume:

- every extended attribute was gone (the archive carried only a "has attributes" flag, and the exporter
  always wrote it zero);
- the access time came back equal to the modification time (the archive carried no access time, and the
  rebuild set both from the modification time);
- the birth time was the successor's own (not carried);
- a pre-epoch modification or change time came back as zero (the archive's times were unsigned, and the
  exporter clamped negatives).

Two snapshots differing only in an attribute or an access time had the same manifest identity, so the
archive's self-verification could not see the loss.

## Root cause

The format (`NodeMeta`, minor 2) never carried the attribute values or the access and birth times, and its
times were `u64`. Its doc said "the attributes themselves are chunks, not manifest bytes", but no code ever
wrote those chunks. Nothing in a placement or a takeover compared metadata, so the gap passed every test.

## Fix (format minor 3)

- **The manifest (`crates/archive/src/manifest.rs`).** `NodeMeta` carries signed `atime_ns`, `mtime_ns`,
  `ctime_ns` and `btime_ns`, and `xattrs: Vec<Xattr>`, each a name and its value's extents over the
  archive's chunks. The flag is gone. The attributes are written in name order and hashed into the identity
  with every other field. The reader refuses:
  - a name that is empty, longer than `XATTR_NAME_MAX_BYTES` (255, Linux's `XATTR_NAME_MAX`) or holding a
    NUL;
  - names that repeat or are out of order;
  - a value whose extents do not tile it;
  - an attribute or extent count past what the bytes left could hold.

  The vfs takes its own name limit from the format's, so a volume can never hold a name its archive would
  refuse.
- **One definition of what an archive references.** `manifest::referenced_chunks` (iterative, root metadata
  included) replaces the cluster's recursive file-only walk. Placement, retention and the `Unreferenced` put
  check now count attribute chunks.
- **Export (`crates/vfs/src/export.rs`).** A node's attribute values are cut into chunks by the same sliced
  cutter as file bytes (an attribute value is an attribute inode's body, §4.5), and the node is placed once
  every value is cut. An empty value has no extents (an empty chunk is not canonical). A node with no
  attributes is placed in the step that began it, so the walk's step count is unchanged for such trees. The
  value is read with `read_in_body`, since the public `read_in` refuses attribute inodes by design.
- **Restore (`crates/archive/src/restore.rs`).** Attribute values are planned bodies like files: admitted
  under the same budget before allocation, each chunk decoded once. `Restored::xattrs` holds them by path
  (the root under `""`) and name.
- **Takeover (`crates/server/src/verbs.rs`).** `restore_node` sets the attributes and the owner, then all
  four times (`Volume::set_times` gained the birth time). A hard link's attributes are set once, at its
  first name.

## Tests

- `an_export_restores_every_attribute_value_and_all_four_times` (vfs; a serial oracle). Every node's four
  times, owner and attribute values match the volume at the snapshot. The cases include:
  - a value of two chunks and a byte;
  - an empty value;
  - the longest name;
  - attributes on a file, a directory and the root;
  - pre-epoch times.

  The first run found the empty-value bug (an empty chunk the decoder refused as non-canonical), fixed in
  this change.
- `an_attribute_or_time_only_change_changes_the_identity` (vfs). A changed value, an added attribute and a
  changed access time each change the identity.
- Archive tests:
  - `the_attribute_encoding_matches_its_golden_vector` (`d1adc5b1…`);
  - the tree golden vector, regenerated deliberately (`d1895ea2…`);
  - `attributes_and_signed_times_round_trip_and_change_the_identity`;
  - `malformed_attributes_are_refused_typed` (hostile input).
- `a_rebuilt_volume_shows_the_origins_attributes_times_and_links` (server, in-process). It goes export →
  encode → decode → restore → `populate_restored`, and every node shows the origin's mode, owner, link
  count, four times (birth time included) and attribute values. Both hard-link names stay one inode.
- `a_takeover_successor_serves_the_dead_owners_content_over_nfs` (fleet, three daemons), extended to the
  audit's acceptance:
  - before the seal, the owner gets attributes on the root and the file over NFSv4.2 SETXATTR, and the
    file's access and modification times over NFSv3 SETATTR;
  - after the owner dies, the successor's root and file must show the same three NFS-visible times and the
    same attribute values over LISTXATTRS/GETXATTR.

  It passes in 10.1 s. With the successor's attribute restore disabled it fails: `attributes: [[], []]`
  against the origin's.

## Siblings reported, not changed here

- **WinFsp creation time.** `crates/bridge-winfsp/src/host.rs` reports `creation_time` from the change time,
  not the birth time, and its SetBasicInfo ignores the creation time it is given (`_creation_time`).
- **Merge engine.** Green volumes rebuild by replaying their merge chain, not through this restore. Whether
  the merge engine's metadata dimension carries all four times is not checked here.
- **Lint wall.** `Cargo.toml` lacks `indexing_slicing`, `string_slice`, `arithmetic_side_effects`,
  `panic_in_result_fn`, `unwrap_in_result` and the assert macros, although CLAUDE.md §2 item 6 says the lint
  wall enforces them (the open indexing sweep).
