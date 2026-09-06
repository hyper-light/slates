# drop_link releases content while an inode is still open, 2026-09-05

Status: fixed. Failing test first (`crates/vfs/tests/lifetime.rs`), then the minimal change in
`crates/vfs/src/volume.rs`. Identified by Ada reviewing the inode-addressed-I/O decision.

## Description
A file unlinked (or replaced by a rename) while a transport still holds it open lost its content
immediately, instead of surviving until the last close (POSIX unlink-while-open).

## Root cause
`Volume::drop_link` (crates/vfs/src/volume.rs), at `nlink == 0`, released the body, removed the
inode from the table and retired the version unconditionally — it never consulted open references,
and the inode carried no reference count for it to consult. The bridge's open-handle table did not
solve this: it maps a handle to an inode for reads; it does not govern inode lifetime.

## Impact
`open` + `unlink` + `read`/`write` on the open descriptor read or wrote reclaimed storage — a
correctness and safety gap. It could not be reached through the FUSE mount yet only because the
bridge did not wire opens to the volume core; the shared inode-addressed-I/O interface (which does)
would have exposed it.

## Fix (exact edits)
`crates/vfs/src/volume.rs`:
- Added a per-volume reference count and orphan set: `references: BTreeMap<InodeNo, u32>` and
  `orphans: BTreeSet<InodeNo>`.
- Added `Volume::reference(no)` and `Volume::unreference(store, no)`; extracted `reclaim_inode`
  (the old unconditional `drop_link` body).
- `drop_link` at `nlink == 0` now defers: if the inode is referenced it is recorded as an orphan
  and kept addressable; otherwise it is reclaimed now. `unreference`, on the last reference of an
  orphan, reclaims at that terminal step.

## Tests (`crates/vfs/tests/lifetime.rs`, every host)
open→unlink→read keeps the content then reclaims at the last reference; rename-over an open file
preserves the replaced inode; with several references the content reclaims only at the last.

## Owed (the rest of the inode-addressed-I/O design, `docs/wip/inode-addressed-io.md`)
Wiring the reference count to the bridge (`open`/`create` reference, `release`/`forget`
unreference), the orphan set's survival across a daemon restart, and the `ObjectId`/`OpContext`
interface change.
