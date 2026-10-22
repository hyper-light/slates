# A mount taken over by a restarted daemon dropped a store made after a re-fetch

**Status:** fixed 2026-10-22 (A-147, approved by Ada in plan mode). Reproduced deterministically before the fix.

**Severity:** silent data loss. A process maps a file through a FUSE mount and stores into it after the mount was
taken over by a restarted daemon (A-61). Where another path changed those bytes after the takeover, the store is dropped
at write-back, even though the process re-fetched the page and stored after seeing the change:
- its `msync` succeeds;
- its map shows the store;
- the volume holds the other path's older bytes.

With DAX through a mount (`docs/wip/virtiofs.md` §8 item 7), a guest's stores are such stores.

**Found:** by the gated guest test `a_daemon_killed_under_a_guests_live_map_is_taken_over_and_the_guest_goes_on`
(`crates/cli/tests/guest_dax.rs`), 3 of 3 runs. Reproduced without a VM by
`a_store_made_after_a_refetch_lands_through_a_mount_taken_over_by_a_restarted_daemon` (`crates/cli/tests/cli.rs`),
3 of 3: the four stored bytes held the other mount's `H` (72).

## What happens

1. A process maps a file through a mount, opened for writing under daemon 1.
2. Daemon 1 is killed; the anchor's successor, daemon 2, takes the mount over (A-61).
3. Another path writes some bytes of the file. Daemon 2 tells the mount's kernel, which drops the page.
4. The process reads the page again. The kernel re-fetches it from daemon 2 (`READ`), and the process sees the other
   path's bytes.
5. The process stores over them and syncs, so the kernel writes the page back (`WRITE` with `FUSE_WRITE_CACHE`).
6. Daemon 2 keeps the stored bytes back as stale, and the store is lost.

## Root cause

The write-back rule (A-140) judges a write-back by byte states the mount keeps for each file open for writing:
- **The states begin at a writable `OPEN` the mount sees** (`WritableOpens::opened`, `PageStates::opened`).
- **A `READ` updates them only for a handle the mount knows** (`state.writable.holds(ino)`).

A mount taken over by a restarted daemon starts with no writable opens (`ServeState::restored`; the successor's handle
store is new). So a file opened before the takeover is never tracked:
- its re-fetches are not recorded;
- its write-backs are judged by the fallback for an untracked file, "fresh at the mount's start against every record
  since" (`stale_in_writeback`, `pending_changes`).

Every byte another path changed since the takeover is then called stale, including bytes the kernel re-fetched after the
change.

**The logs** (temporary, on the box, 2026-10-22):
- before the kill, the guest's file was tracked (`tracked=true`), and its keep-backs were the other path's bytes;
- after it, the restored mount logged `tracked=false started_at=0` and kept back `[10586..12288)` and `[12288..14596)`.
  The first range holds byte 12187, where the guest's later store was lost.

## Impact

Any file held open for writing across a takeover:
- a store made after a re-fetch of bytes another path changed since is lost;
- a long-lived map is refused `EIO` once the journal no longer reaches back to the mount's start (recorded in GAPS as
  "A restored mount's files"; the lost store was not).

## The fix (A-147)

On a restored mount, a `READ` or `WRITE` that names a handle the mount never saw opened adopts its file into the byte
states:
- **fresh as of the mount's start**, the assumption the fallback already makes for every byte;
- **then the file's journal changes since the start applied**, which the mount's rounds consumed while the file was
  untracked. Without them, the new states would call a byte fresh that another path changed in between.

If the journal no longer reaches back to the start, the file is lost, and its write-backs are refused `EIO` as before.
From then on the file is judged as one opened on this mount:
- re-fetches make bytes fresh;
- other paths' records make them stale, fed from each round;
- so the past-retention `EIO` goes too.

The handle is forgotten at its `RELEASE`, like any other. A read-only handle adopted this way costs its file's states
until released; nothing writes back through it.

## Exact edits

- `crates/bridge-fuse/src/coherence.rs`:
  - `WritableOpens` keys a handle by its value and its file. A successor's handle store starts its generations at 0
    (`new_handle_store`), so it reissues the dead daemon's values, and one value can name two files. `released` takes
    the file, and `knows(fh, ino)` is new.
  - `PageStates::adopted(ino, start, changes, bound)`: a file tracked from here on, fresh as of `start`, with the
    journal's changes to it since then applied; lost when the journal no longer reaches back.
  - Unit tests:
    - `an_adopted_file_is_judged_as_untracked_until_a_refetch_and_precisely_after`;
    - `a_file_adopted_past_the_journals_reach_is_lost_until_it_is_dropped`;
    - `a_reissued_handle_value_is_told_apart_by_its_file`.
- `crates/bridge-fuse/src/channel.rs`:
  - `ServeState.adopts_unseen_handles`, true only from `ServeState::restored`;
  - `adopt_unseen_handle`, called before a `WRITE` is served (`written_handle`, in `dispatch_request`) and after an
    answered `READ` (`note_read`, split out of `note_opens_and_fetches`);
  - a `RELEASE` and `forget_unreported_open` name the file as well as the handle.
- `crates/cli/tests/cli.rs`: `a_store_made_after_a_refetch_lands_through_a_mount_taken_over_by_a_restarted_daemon`.
- Docs: `SLATES_DESIGN.md` §4.6 and A-147; GAPS.

## Verification

| | before the fix | after |
|---|---|---|
| the CLI test (box, Linux 7.2.9) | red 3 of 3: bytes 4106..4109 hold the other mount's `H` | green 3 of 3, 1.4 s each |
| the same, adoption turned off in the fixed tree | red, the same four bytes | — |
| the guest takeover test (box, crosvm) | red 3 of 3 at byte 12187 | green, with 11 checks across the kill |

- **The regressions on the box are green:**
  - bridge-fuse's suites, the `coherence_mount` chaos tests among them;
  - the server's `fuse_mount`, 3 of 3;
  - the whole CLI suite, 51 of 51, its map tests and held-mount tests (A-61, A-63) among them;
  - the three guest tests.
- **The coherence unit tests pass on macOS too,** 10 of 10.

## Siblings found

1. **The start point.** An adopted file is judged from the journal's head at the restored mount's first request, as
   the fallback was. Records the successor made before then are outside it: the write log's replay (A-63, journaled
   by `Volume::write`) and any other path's write before the mount's first request. Counting them would call the
   mount's own replayed writes stale, so they stay outside. That is part of the takeover gap (GAPS), Ada's decision.
2. **`VolumeBridge::release` resolves a handle by its packed value alone,** ignoring the node it is given
   (`crates/bridge-core/src/volume_bridge.rs`). With the successor reissuing the dead daemon's values, a pre-takeover
   handle's `RELEASE` can close a post-takeover handle of another file, dropping that file's open reference early.
   Not yet reproduced; next is a failing test, then a check of the handle's inode against the request's node.
