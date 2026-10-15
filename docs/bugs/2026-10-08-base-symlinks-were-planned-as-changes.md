# Every untouched base symlink was planned as a change, and a fresh listing dropped the volume's own symlinks

**Found:** 2026-10-08, measuring condition 5 on Linux (`docs/wip/bench/overlay/overlay_diff.sh`, an overlay of
`/usr/lib`). **Status: fixed** (same day; A-121).

## Description

An overlay volume of `/usr/lib` (5,863 files, 657 symlinks) was given 10 appends, 5 creates and 3 removals through the
Linux NFSv4.2 mount. `slates land` then planned `create=5 delete=3 replace=10 symlink=657`. Each of the 657 symlinks
was one the volume had only read. The macOS run of 2026-10-05 over `crates/` planned exactly 18, because that tree
holds no symlinks.

## Root cause

`materialize` (`crates/vfs/src/base.rs`) loaded a base symlink as `Body::Symlink(target)`, the body a symlink the
volume creates also has. The diverged walk (`diverged_from`) counts an unwitnessed entry as base only when its body is
`Body::Base`, so every base symlink was reported `Created` and planned as `Action::Symlink`.

The same ambiguity ran the other way in `unloaded_file`, which took any unwitnessed `Body::Symlink` with one link for
an untouched base entry. `stale_entries` drops such an entry from a merged directory when a fresh listing no longer
holds its name. A symlink the volume created is never in the disk's listing, so it was dropped the next time an
outsider's change made the directory be listed again. Found by reading; the test below reproduced it.

Two smaller faults in the same place:
- A base symlink took neither the disk's modification time nor its change time; it reported time zero.
- An outsider retargeting an untouched base symlink (`ln -sf`) never showed through: its target was read once.

## Impact

- Landing: a plan for an overlay of any tree with symlinks carried one spurious entry per symlink the volume had
  listed. The verdict for each was a skip (a symlink over a symlink), so nothing wrong was written, but the plan a human
  grants was not the change set, and its manifest hash covered entries nobody made.
- Data loss: an agent's `ln -s` in a base directory disappeared after any outsider change to that directory followed
  by a listing. No error was returned.
- `stat` on a base symlink reported mtime and ctime zero; a retarget on the disk stayed invisible.

## Fix (exact edits)

- `crates/vfs/src/inode.rs`: `Body::BaseLink(Box<str>)`, the untouched base symlink, the symlink counterpart of
  `Body::Base`.
- `crates/vfs/src/base.rs`:
  - `materialize` builds `Body::BaseLink` with the disk's times;
  - `diverged`, `diverged_in` and `unloaded_file` count `Body::BaseLink` as base, and a plain `Body::Symlink` as the
    volume's own;
  - `copy_up` turns a `Body::BaseLink` into a `Body::Symlink` (rename, link and setattr call it), so a renamed base
    link lands at its new name, with the whiteout at the old one;
  - `stale_entries` drops an untouched base link whose listed entry changed kind, size or times (`listed_link_changed`),
    and the next lookup serves the disk's.
- `crates/vfs/src/volume.rs`: `readlink` and `readlink_in` answer for both bodies.
- `crates/vfs/src/recover.rs`: `BodyImage::BaseLink`, appended, so earlier images decode as before.

Tests, each failing before the fix (`crates/vfs/tests/base.rs`):
- `an_untouched_base_symlink_is_not_a_change`;
- `a_symlink_the_volume_created_survives_a_fresh_listing`;
- `an_untouched_base_symlink_recovers_untouched`;
- `a_renamed_base_symlink_is_a_removal_and_a_creation` (fails when the `copy_up` branch is disabled: the renamed link
  vanished);
- `an_untouched_base_symlink_follows_the_disk`.

After the fix, the same bench planned `create=5 delete=3 replace=10` in 2–3 ms, three rounds of three.

## Siblings checked

- Untouched base files can also change kind on the disk (a file replaced by a directory). `stale_entries` refreshed a
  file only while its listed kind was still a file, and the other kinds fell through unchanged; an empty merged
  directory the disk made a file stayed a directory too. Fixed in the next commit the same day: both are dropped at the
  relisting, with a failing-first test each.
- Devices, fifos and sockets on the base are `HostKind::Other`: listed, never served (`crates/vfs/src/host/mod.rs`), so
  no base body exists for them to be confused with.
