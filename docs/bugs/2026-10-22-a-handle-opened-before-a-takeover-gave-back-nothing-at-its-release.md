# A handle opened before a takeover gave back nothing at its release

**Status:** fixed 2026-10-22 (A-152, approved by Ada in plan mode). Reproduced by four failing seam tests and, end to
end, on the box (Linux 7.2.9).

**Severity:** a leak bounded by the mount's lifetime. After a FUSE daemon takeover (A-61), the release of a handle the
dead daemon issued gives back nothing. So:
- the open reference that handle held stays until the mount ends;
- a file unlinked while open is never reclaimed when the kernel lets go of it;
- its bytes stay charged and its inode stays in the table.

No bytes are wrong and nothing is reclaimed early: the failure is in the safe direction, and it accumulates with every
pre-takeover handle on a long-lived mount. Recorded in GAPS since A-148 ("a pre-takeover handle's release frees nothing
— open, a bounded leak").

## Reproduction

`crates/bridge-core/tests/takeover_release.rs`, four tests, each red at its last assertion. Each one:
1. creates a file through the dead daemon's bridge, with the lookup reference the FUSE edge takes for it;
2. opens it, and unlinks it while open;
3. publishes (the image), then rebuilds the volume and settles its references as recovery does;
4. serves the releases through the successor's bridge, whose fresh handle store never issued those handles;
5. then forgets the kernel's lookup.

| test | what it covers | red at |
|---|---|---|
| `a_release_of_a_handle_opened_before_a_takeover_gives_back_its_reference` | one handle | "reclaimed once the kernel lets go" |
| `a_release_after_a_takeover_never_gives_back_a_reference_another_handle_still_needs` | three handles, and a release repeated | "reclaimed at the forget" |
| `a_handle_opened_after_the_last_publication_gives_back_no_more_than_the_image_restored` | one handle opened after the publication | "reclaimed at the forget" |
| `a_reissued_handle_value_on_the_same_file_balances_across_the_takeover` | the successor's own handle under the same value (A-148) | "reclaimed at the forget" |

In each, the file is still readable after the kernel's forget: an open reference outlived every handle.

## Root cause

`VolumeBridge::release` (`crates/bridge-core/src/volume_bridge.rs`) frees a handle only when its slot holds the file the
release names (A-148). A handle the successor never issued is released as a no-op.

The no-op was deliberate. The image carries each recorded attachment's references as one count per inode, with lookups
and opens together (`InodeReferences { inode, count }`; an open takes a reference). The successor therefore cannot tell
whether an open's reference was restored, since references are durable only once a publication captured them.
Dropping one it never restored would take a lookup reference instead. Open counts themselves (`Volume::opens`) are never
published.

## A second cause, found by the fix

The first build carried open counts in the image and still failed all four tests. Only `OPEN` ever counted an open
(`Volume::open_for`). A `CREATE`'s handle and an `OPENDIR`'s took a reference through `open_handle` and counted no open,
so the image had nothing to restore for them, and the tests create their file. Every handle `open_handle` issues now
counts its open.

## The fix (A-152)

- **Every handle is an open**, a file's `OPEN`, a `CREATE` and an `OPENDIR` alike (`open_handle` records it).
- **The image and the delta carry each recorded attachment's open count** beside its references
  (`InodeReferences::opens`, `ReferenceChange::opens`; image layout 21). An open takes a reference, so more opens than
  references is a corrupt image, refused.
- **A recovery gives the opens back as restored opens** (`Volume::restored_opens`).
- **The release of a handle the successor never issued** consumes one restored open of its file and attachment,
  closing it and dropping its reference (`Volume::release_restored_open`); with none left it gives back nothing.

It never reclaims a file the kernel still holds. The kernel cannot forget an inode it holds open, so a publication that
captured an open of a file also captured a lookup reference, which holds the file until the kernel's `FORGET`, whichever
handle's release consumes a restored open. A handle opened after the last publication left nothing in the image, so its
release takes at most a sibling's restored open, and all the releases together never more than the image restored.

## Exact edits

- `crates/vfs/src/recover.rs`: `InodeReferences::opens`; recovery passes it to `restore_references`; `IMAGE_VERSION` 21.
- `crates/vfs/src/delta.rs`: `ReferenceChange::opens`, written by both encoders and set by `apply_reference`.
- `crates/vfs/src/volume.rs`:
  - `restored_opens`;
  - `attachment_open_count`;
  - `attachment_references` returns the image's records with their opens;
  - `restore_references` takes the open count and refuses more opens than references;
  - `release_restored_open`;
  - `open_for` and `close_for` record a changed reference;
  - `sweep_attachment` drops the owner's restored opens.
- `crates/bridge-core/src/volume_bridge.rs`: `open_handle` counts every handle open; `release` gives back a restored
  open for a handle it never issued.
- Tests:
  - `crates/bridge-core/tests/takeover_release.rs` (four, new);
  - `crates/vfs/tests/recover.rs`: `a_delta_carries_each_open_count_and_replays_to_the_full_image`, and more opens than
    references refused;
  - `crates/cli/tests/cli.rs` `the_handles_a_dead_daemon_issued_give_back_their_references_at_release`.
- Docs: `SLATES_DESIGN.md` A-61 and A-152, GAPS.

## Verification

| | before | after |
|---|---|---|
| the four seam tests | red at their last assertion ("reclaimed …") | green |
| the CLI test (the box), an unlinked 1 MiB file whose two descriptors closed after a takeover | 1,048,576 bytes still referenced after the 10 s wait | `referenced_bytes` back to 0 at once (0.40 s for the whole test) |
| an image recording more opens than references | not representable | refused `RecoveryIncomplete` |
| a delta changing open counts, replayed onto the last image | not representable | equal to the full image; streamed equals built |

- **macOS:**
  - the vfs, bridge and land suites: 922 of 922 (the counts reconcile with the run before: −18 `slates-base`, not run,
    +5 new);
  - the server's `recovery` (39), `nfs_mount` (40), `virtiofs` (8) and `attach_forms` (9);
  - clippy `-D warnings` on the changed crates.
- **The box (Linux 7.2.9, load average 7 to 26):**
  - workspace clippy `-D warnings`;
  - the vfs, bridge, land and base suites: 971 of 971;
  - the CLI suite: 55 of 55;
  - the server's `fuse_mount` (3), `fuse_idle` (1), `nfs_mount` (40), `recovery` (41) and `virtiofs` (18);
  - `coherence_mount` (7) and the guest tests (3).
- **Linux without `/dev/udmabuf`** (a native arm64 container, bounded to 16 GB and 4 build jobs): the virtio-fs and
  bridge-core suites, 192 of 192, the device suite's half the box skips among them.
- **Found by Linux clippy, not macOS:** the CLI test first called `std::thread::sleep`, disallowed by the lint wall. The
  test is Linux only, so the Mac's clippy never compiled it. It uses the file's `pause()` now.
