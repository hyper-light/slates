# A daemon kept a descriptor per base file it ever read, and a recovery reopened every directory at once (2026-11-21)

## Description

kiln's Slates daemon on adacachy reached 1,048,575 of its 1,048,576 file descriptors and refused every open (`EMFILE`):
kiln's `up()` failed and a second anchor exited 4. Its workspace volume overlays ~/Projects/kiln, with 731 agent
worktrees under `.claude/worktrees`. This MacBook's kiln daemon held 4,574 (3,995 regular files of ~/Projects/slates,
531 directories). After a restart, the adacachy daemon was killed by the liveness watchdog about a second into every
start, and the anchor counted each restart as recovered.

## Root cause

1. `BasePlane::descriptors` (crates/vfs/src/base.rs) kept the descriptor every read-through opened, by inode, for the
   life of the daemon. Only a reclaim (unlink) or a replaced name (A-151) closed one. A tool walking a tree (rg, grep,
   cargo) opened every file once, and each descriptor stayed.
2. `BasePlane::recover` (crates/vfs/src/base_recovery.rs) reopened every directory the image recorded, a path component
   at a time from the root, and fingerprinted each one inside the shard's single start step. Every directory ever
   listed is recorded, so adacachy's daemon reopened about 192,000 in one step and overran the 1 s liveness window
   (machine/src/budgets.rs).

## Impact

Every Slates daemon whose volumes were read widely ran out of descriptors in time, on every platform. A restart could
not recover a volume with a large listed tree. kiln's workspaces on adacachy were down. kiln's fleet server answered 503
while its Slates instance looped.

## Edits

- A-832 (942d4a37e): an untouched base file's descriptor is held only while a handle holds the inode open.
  - `BasePlane::transient`: opened by a verb with no open; closed when its overlay borrow ends.
  - `BasePlane::deferred`: `open_base`'s descriptor, judged one borrow later, after `Volume::open_for`.
  - `BasePlane::release_unopened`, from `Volume::unpin_open`: at the last close.
  - Witnessed large copies keep theirs.
  - Tests: `reading_base_files_keeps_no_descriptor_after_the_reads` (506 -> 6 handles after 500 reads),
    `an_open_base_file_keeps_its_descriptor_until_its_last_close`.
- A-833 (6e3dec78d): a recovery reopens only the root. Each other recorded directory keeps its record (`Listing::dir`
  `None`, `Listing::recorded`) and is reacquired at first use (`Overlay::reacquire`: source path, `same_object`, else
  `BaseUnavailable`). `image()` writes an unreacquired directory's record with no `fstat`. Test:
  `a_recovery_reopens_only_the_root_and_reacquires_each_directory_at_its_first_use` (201 -> 1 handles).
- kiln carries both as vendored change 0003 (kiln 76ec14407).

## Siblings, not fixed here (GAPS)

- A directory listed in a daemon's life keeps its listing and open directory until it leaves the volume.
- The anchor counts a daemon that died before it served as recovered, with no back-off.
- Witnessed descriptors are bounded only by the large-class copies a volume holds.
- `image()` still `fstat`s every reacquired directory at each publication.
- A hint is matched to its listing by a scan of every listing.
