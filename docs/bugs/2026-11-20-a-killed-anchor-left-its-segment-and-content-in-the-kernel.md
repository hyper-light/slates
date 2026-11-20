# A killed anchor left its segment and content object in the kernel on macOS (2026-11-20, A-778)

## Description
Found by use on the big Studio (adas-mac-studio.local, 96 GB). Four pushes of Mercurial's history (84 MB store), each
to a fresh `slates anchor --vcs-http`, each anchor then stopped with `SIGKILL` as `crates/cli/bench/hg_scale.sh` stops
its servers. The system's free memory fell 44,904 -> 33,801 -> 24,567 -> 11,692 -> 3,517 -> 58 MiB. No slates process
was alive, yet about 25 GB of active and inactive pages belonged to no process. Removing the eight objects' names
(`shm_unlink`, the names from each anchor's `issuer surface: export` line) took free memory from 2.8 to 23.7 GiB at once.

## Root cause
On macOS a POSIX shared-memory object lives while its name exists, whoever holds it. The anchor creates its segment
(`SLATES_ANCHOR`, 45.9 GB of address space) and its content object (`SLATES_ANCHOR_CONTENT`, 146.0 GB) and only the
creator removes a name, when it drops the object (`slates_mem::shared`). A `SIGKILL`ed anchor never drops it. Its daemon
sees the anchor gone and leaves (`slates_api::role::daemon::serve`), but it only opened the objects, so it removed
nothing. Every page the daemon had written stayed in the kernel until reboot. Linux hands these objects by descriptor
(memfd) and Windows' sections close with their last handle, so only macOS leaked.

## Impact
Every macOS anchor ended by a signal it cannot catch leaked its whole working set: about 5 GB a run after an hg history
push. That covers bench harnesses, tests whose `AnchorProcess` guard kills the anchor on drop, and crashes. It is the
likely reason the shared Studios ran short of memory and swapped.

## Exact edits
- `crates/mem/src/shared.rs`: `unlink_handoff` removes a named handoff's name on macOS (an absent name is no refusal);
  it does nothing elsewhere. Re-exported from `slates_mem`.
- `crates/api/src/role/daemon.rs`: a daemon whose anchor is gone calls it for `SLATES_ANCHOR` and
  `SLATES_ANCHOR_CONTENT` before it stops (`release_the_anchors_objects`). Nobody adopts them after the anchor: a new
  anchor creates its own under a new name.
- `crates/cli/tests/cli.rs`: `a_killed_anchors_segment_and_content_go_with_its_daemon` (macOS; red before the fix: both
  names outlived every holder).

## Siblings, reported, not fixed here
- A daemon killed together with its anchor (both `SIGKILL`ed before it notices) still leaves both names. A next anchor
  cannot find them, because each anchor names its objects afresh. The harnesses should stop anchors with `SIGTERM`.
- The anchor's other named objects (the client rendezvous, held copies) were not measured.
