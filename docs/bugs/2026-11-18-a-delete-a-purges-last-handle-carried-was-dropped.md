# A delete that a coherent purge's handle carried was dropped, so the deleted name stayed in the volume

**Date:** 2026-11-18. **Area:** `crates/bridge-winfsp/src/host.rs` (`run`'s cleanup, `serve_operation`'s barrier,
`change_of`); design §4.6 (Windows), A-190 (the coherent purge), A-360 (hard links). **Found by:** A-360's live test
`a_hard_link_made_through_the_drive_names_the_same_file` on the test-signing Windows VM (adacachy, the patched WinFsp
driver 2.1.26322.8570a40, slates at `a94026fa8`). It failed once in three runs: `one link left after the first name's
delete: not seen within 10s`. The two other runs saw the other name count one link 21.6 and 23.1 ms after the delete.

## Description

The test links `second.txt` to `first.txt` and writes through `second.txt`. That write leaves `first.txt` owed a
notification and a purge of the written bytes (A-360's owed words, A-190's purge). The test reads the write back
through `first.txt`, deletes `first.txt` with `DeleteFileW`, which succeeds, and then waits for `second.txt` to count
one link. In the failed run the count stayed at two for the whole 10 s bound.

## Root cause

WinFsp gives a pending delete to the cleanup of the file node's last handle, whoever opened that handle. In v2.1's
`src/sys/file.c`, `FspFileNodeCleanup` sets `Delete = DeletePending` only when `1 == FileNode->HandleCount`. A
POSIX-semantics delete is refused on this volume, which does not declare `SupportsPosixUnlinkRename`
(`src/sys/fileinfo.c` answers `FileDispositionInformationEx` `STATUS_INVALID_PARAMETER`). So `DeleteFileW` falls back
to the classic disposition, and its delete rides on the last handle's cleanup.

A coherent purge holds such a handle. The purge client opens the changed file through the drive, writes and reads its
range, then closes it (`crate::purge_client`). When a program deletes the file while that handle is open, the purge's
cleanup is the last one, so it carries `FspCleanupDelete`.

The shard answered every cleanup of a purge's open `Ok` without looking at its delete (`run`: `if object.purge {
Reply::Ok }`). WinFsp completed the delete on its side: it notified the name's removal and dropped the file node. The
volume kept the name. The deleter was told the file was gone, and the next lookup of that name found it again.
`change_of` passed over a purge's cleanup too (`if !object.purge`), so the file's other names were never told that
the link count fell.

The new shard test `a_delete_a_purges_last_handle_carries_is_made` serves exactly that cleanup. Red-checked once, in
diagnostics run 44359955657 (`diag-g258-22`) on both Windows runners. With the fix's `host.rs` change reversed, the
test fails at its first assertion: "the delete the purge's last handle carried was not made". With the change, it
passes, beside the 21 link tests and the 5 other shared-change tests.

On the test-signing VM, a race (`a<n>` linked to `b<n>`, `a<n>` held open, a write through `b<n>`, then `a<n>`
deleted right after its close) showed the same loss before the fix. The volume kept 21 of 28 deleted names in one
run and 2 of 16 in another (`a94026fa8`). With the fix it kept 0 of 59 (`35eb55dc5`).

## Impact

This predates hard links. It holds since A-190's purge client (2026-11-06) for any file that is purged after a change
from outside its open names (another attachment, the SDK, a second route, a base file changed on disk). A delete that
lands while the purge's handle is open is lost: the name comes back. The window is one purge, from the client's open
to its close, a few milliseconds a file. Hard links widen it, because every write through one name purges the others
(A-360). No data is lost: the file keeps its bytes. What is wrong is that a name the program deleted still exists.

## Exact edits

- `run`, `Op::Cleanup`: the delete is made, purge or not (`op_delete`). A cleanup without one is answered `Ok`, as
  before.
- `serve_operation`: only a purge's cleanup that carries no delete skips the publication barrier. A carried delete is
  published before its reply, as every delete is.
- `change_of`: a delete is a change whoever's cleanup carried it, so the file's other open names are owed the new link
  count (A-360).
- The host's module doc says a purge's cleanup can carry another caller's delete.
- Test: `a_delete_a_purges_last_handle_carries_is_made` (`crates/bridge-winfsp/tests/support/host.rs`), failing first.

## Seen beside it, not fixed here

The same VM race also hung the deleter, before the fix and after it. That hang is a separate open gap (GAPS, "A
delete racing a coherent purge can hang its caller"). Three deletes of 59 were also refused `ERROR_ACCESS_DENIED`
with the fix in place, where NTFS would delete; their cause is open with the hang.

## Siblings swept

Every other branch on `object.purge` in the host was checked:
- A purge's write is an echo, never applied. That is its own effect.
- A purge's open and release open and close only the purge's own handle.

WinFsp's other last-handle cleanup flags (`SetAllocationSize`, `SetArchiveBit`, `SetLastWriteTime`, `SetChangeTime`)
are not acted on for any handle; the shard's cleanup carries only the delete. So none of them can be dropped for a
purge alone.
