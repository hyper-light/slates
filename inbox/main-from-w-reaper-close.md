# w-reaper -> main: close (2026-11-21 ~00:00 PST)

## Items closed
1. **The issuer route's EINTR fix (C1):** 2e7756638 (main's note of 20:00).
2. **A-645 (a), svn's revprop rows:** a configured Subversion hook's REPOS-PATH is its repository's FSFS view,
   presented on the owner shard and mounted read-only under the store's standing grant (`<--vcs-store>/<repository>.svn`).
   - f532cdd54: the view laid into a volume of referenced files (`vcs_store::svn_presenter`, `fsfs::RevisionFile::pieces`).
   - 1c5b77fc1: `StoreRoot::mount_point`; every view file a referenced file, so the chunk store stays empty.
   - 5c90dbcbe: REPOS-PATH is the mounted view (`HookViews`, `repos_for_hook`, `hook_driver::run_helper`,
     `VolumeBridge::reading_referenced`, the NFS and FUSE lends); the "root mount not yet published" refusal gone.
   - ef3247236, landed by w-interop as f09d464ec and a51023f57 after its r6 run passed:
     - the view sized to its own content (`Presented::size_for`, R3; it had been `max: 0`, hence `Volume(NoSpace)`);
     - every view entry owned by the view root's owner, directories 0755 and files 0444 (they had been uid 0 and
       unreadable to the hook);
     - slates-store's memtable bounded by its 32-bit offsets (a shard reserve past 4 GiB had refused every landing).

## Runs
- Mine (macOS arm64, Studio-2):
  - `slates-server --lib` svn, git push, tunnel, DAV, hook driver and presenter: 67/67, then 49/49 on the merged tree.
  - `vcs_store::svn_presenter` 2/2, red at a ceiling of nothing (`NoSpace`) and with a non-root owner (uid 0).
  - `slates-store` all, red: `a_store_lent_more_memory_than_a_memtable_addresses_lands_and_reads`.
  - `slates-land --test store_root` 4/4; fmt clean.
- By use: w-interop's r6, which landed ef3247236. Mine never ran: my runner's kiln could not reach the fleet.
- A-748 Windows check: not finished. jupiter's debug build ran past r1's two-hour bound, and earlier runs were lost to
  runner restarts. The logs point to w-interop's own teardown (your 4b335a696), not a lost port. The run request
  stays in run-requests.md.

## Left in GAPS (noted, not worked)
- Each hook refreshes every revision's revision properties file: linear in the history, a revision a turn, until a
  revision property change gives its own signal.
- A view is not unmounted when its repository goes.
- Linux clippy of `fuse.rs` `turn_with`'s `lent_if` closure: in run-requests.md for w-lint.

## Found by use, for someone else
- Studio-2's kernel NFS client is wedged by my earlier hung test mounts: even cli.rs's plain
  `slates_mount_establishes_a_real_kernel_mount_and_unmount_removes_it` hangs there now. A reboot clears it.
