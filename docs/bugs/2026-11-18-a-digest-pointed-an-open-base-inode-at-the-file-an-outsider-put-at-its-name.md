# A digest pointed an open base inode at the file an outsider put at its name (2026-11-18)

Contracts: §4.15 "verified current" (a clean digest names exactly the current immutable bytes at the path), §4.5 and
A-151 (an untouched base inode is bound to the disk file it was made from; when the disk gives its name to another file
the inode leaves the name, open or not, and the name is a new entry).

## Description

Found by g225's inventory (GAPS, Shards part 3 step 4's open items): `verify_current` re-pointed an untouched inode to
another file found at its name, where A-151 detaches the inode, and no test reached the path.

`verify_current` validates the directory's listing, then opens the path afresh and compares its identity with the
descriptor the inode holds. A replacement that lands between the two leaves the listing's fingerprint as it was, so only
this check sees it. On a mismatch the code swapped the inode's descriptor for the new file's and adopted the new
fingerprint. From then on, a handle opened on the old file read the outsider's bytes, and the name kept the old inode
number. A-151 (2026-10-22) had made every relist detach such an inode instead.

## Root cause

A-151 changed the relist (`prune_unloaded`) and the open (`descriptor`, `open_base`), and left this third place that
finds a name given to another file, so its in-place adoption, older than A-151, survived.

## Impact

A process that held a base file open while an outsider (an editor's save-by-rename, a `git checkout`) replaced it could
read the new file's bytes through its old handle after any digest of the path found the replacement, the A-151 bug
again in a narrower window. A digest by path itself named the right bytes either way.

## Exact edits

`crates/vfs/src/base.rs`:
- `verify_current` answers `Current` (`Verified` or `Replaced`). On an identity mismatch, `leave_name` applies A-151 as
  a relist does (`release_detached`, the listing invalidated, `drop_unloaded`), counted `DigestStats::replaced`. An
  inode the validated listing already took from its name answers `Replaced` too, where `home_of` said `NotFound`.
- `digest_begin` looks the name up once more on `Replaced` and digests the new entry; twice is `DigestUnverified`.
  `reopen_verified` refuses `Replaced` as `DigestUnverified`.
- A unit test module (the crate's first in `base.rs`): the race at every seam call of a digest.

## Test

`a_digest_racing_a_replacement_leaves_the_opener_on_its_file_and_names_the_new_one`, red without the fix (the handle
read the outsider's file), green with it.
