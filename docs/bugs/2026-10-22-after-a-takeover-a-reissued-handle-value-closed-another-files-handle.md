# After a takeover, a reissued handle value closed another file's handle

**Status:** fixed 2026-10-22 (A-148, approved by Ada in plan mode). Reproduced deterministically at the bridge seam before the fix.

**Severity:** data loss after a takeover (A-61). The kernel releases a handle it opened before the takeover. When the
successor has meanwhile issued the same handle value for another file, the release closes that file's handle instead:
- its open reference is dropped;
- if the file was unlinked while open, it is reclaimed under its open handle, its reads answer `NotFound` and its bytes
  are gone.

There is a second effect. A pre-takeover handle of a file that lost writes (A-61's `LostWrites`) is not told `EIO` at
its `fsync` when the successor has issued the same value, so the loss goes unreported.

**Found:** by reading, as a sibling of A-147
(`docs/bugs/2026-10-22-a-mount-taken-over-by-a-restarted-daemon-dropped-a-store-made-after-a-refetch.md`), then
reproduced by two seam tests in `crates/bridge-core/tests/volume_bridge.rs`:
- `a_release_of_a_reissued_handle_value_never_closes_the_successors_handle_for_another_file` failed with
  `the unlinked file is still open through its own handle: Err(NotFound) []`;
- `a_lost_write_reaches_a_pre_takeover_handle_whose_value_the_successor_reissued` failed with `left: Ok(())`,
  `right: Err(RecoveryIncomplete)`.

Each first asserts that the two handle values are equal, so neither can pass vacuously.

## Why the values repeat

A mount taken over by a restarted daemon is served with a fresh handle store (`new_handle_store()` in
`crates/server/src/fuse.rs` `adopt_one`). It starts its generations at 0, as its predecessor's did, so the successor
issues the values its predecessor issued: the first handle of each is slot 0, generation 0. The kernel still holds the
predecessor's handles. `Slab::with_generation_base` exists for one store replacing another, but a takeover cannot use
it: the dead store's highest generation is gone with its process.

## Root cause

- `VolumeBridge::release(object, cx, fh)` resolves the handle by its packed value alone
  (`self.handles.get(unpack_handle(fh))`). It ignores the file it is given, and removes the slot unconditionally.
- `LostWrites::issued` records the successor's handles by value alone, and `reports` treats any value it issued as the
  successor's own.

## The fix (A-148)

- **A release acts only on a handle of the file it names.** A value whose slot holds a handle of another file is one
  this bridge did not issue for that file: a handle from before a takeover. Its release touches no handle of this
  bridge, as an unknown value's release does today.
- **`LostWrites` keys the successor's handles by value and file.**
- **A release names the node its open resolved.** `serve_release` (`RELEASE` and `RELEASEDIR`) and the lost-reply
  reclaim resolve node id 1 to the volume's or scope's root, as `serve_open` did. Without that, the file check would
  refuse every root `RELEASEDIR` and leak a slot per listing, until `EMFILE`.

## What it leaves

A pre-takeover handle's release still drops nothing. Its open took a reference, and references are durable only once a
publication captured them. So the successor cannot tell whether that reference was restored, and dropping one it never
restored would take a lookup reference instead, reclaiming a file the kernel still holds. That is a bounded leak until
the mount ends, recorded in GAPS.

Not shown on a real mount: handle values are invisible to processes. The seam tests drive two handle stores as a
predecessor and its successor do.

## Exact edits

- `crates/bridge-core/src/volume_bridge.rs`:
  - `release` frees a handle (close, forget one reference, free the slot) only when its slot holds the file the release
    names, and tells the lost-write record either way;
  - `LostWrites`: `issued` and `released` take the file, and `reports` checks the pair.
- `crates/bridge-fuse/src/bridge.rs`: `serve_release`, and `release` in `reclaim_unreported`, resolve the node.
- The tests:
  - `crates/bridge-core/tests/volume_bridge.rs`: the two seam tests and their fixture;
  - `crates/bridge-fuse/tests/volume_bridge.rs`: `releasing_the_root_directory_frees_its_handle_round_after_round`.
- Docs: `SLATES_DESIGN.md` §4.6 and A-148; GAPS.

## Verification

| | without the fix | with it |
|---|---|---|
| the release seam test | `Err(NotFound) []` | green |
| the lost-write seam test | `left: Ok(())`, `right: Err(RecoveryIncomplete)` | green |
| the release test, file check removed | `Err(NotFound)` again | — |
| the root-release test, resolve removed | round 2 refused `-24` (`EMFILE`) | green, 8 rounds |

- **macOS:** bridge-core, bridge-fuse and bridge-nfs suites; the server's `nfs_hostile`, 17 of 17.
- **The box:**
  - clippy `-D warnings` (bridge-core, bridge-fuse, cli, server);
  - the bridge suites with the real-mount tests;
  - the server's `fuse_mount`, 3 of 3;
  - the whole CLI suite, 51 of 51;
  - the guest tests, 3 of 3.
