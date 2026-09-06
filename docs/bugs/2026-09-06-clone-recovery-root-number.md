# 2026-09-06 — a recovered clone got the wrong root inode number

## Description

`Volume::from_image` (§4.8 recovery) rebuilt a clone with a root directory numbered
`compose(clone_prefix, 1)` instead of the root the clone actually has. A clone inherits its
origin's tree, so its root is the origin's root — `compose(origin_prefix, 1)` — which the image
records in `root_no`. The round-trip oracle caught it: `to_image(from_image(clone_image))`
differed from `clone_image` in `root_no` and in the root inode's number (e.g. `compose(8, 1)`
recovered where the original was `compose(7, 1)`).

## Root cause

`Volume::recovery_shell` (crates/vfs/src/volume.rs) computed the root inode number itself as
`InodeNo::compose(seed.prefix, 1)`. That is correct for a scratch volume, whose root is
`compose(prefix, 1)`, but wrong for a clone, whose root number carries the origin's prefix. The
image already carried the correct number in `root_no`; the shell ignored it and recomputed.

## Impact

Clone recovery only. A recovered clone's root directory would have a different inode number than
before the restart, so a client's handle to the clone's root, and its `..` resolution, would not
match — a faithful-recovery violation for clones. Scratch and snapshot recovery were unaffected
(their root is `compose(prefix, 1)`, so the recomputed number happened to be right). No released
data was at risk: clone recovery had no test and was not yet claimed.

## Fix

`VolumeSeed` gains a `root_no: InodeNo` field; `recovery_shell` uses `seed.root_no` for the root
inode instead of recomputing it, and `from_image` sets `seed.root_no = InodeNo(image.root_no)`.
The image's recorded root number is now authoritative for the rebuilt root.

Exact edits:
- crates/vfs/src/volume.rs: add `VolumeSeed.root_no`; `recovery_shell` uses `seed.root_no`.
- crates/vfs/src/recover.rs: `from_image` sets `root_no` in the seed.

## Test

crates/vfs/tests/recover.rs `a_clone_recovers_inherited_and_diverged_content`: a clone of an
origin snapshot, diverged with its own file, re-images byte-identically after rebuild, and both
the inherited and the diverged content read back. It fails on the pre-fix `recovery_shell`
(root-number mismatch) and passes after. Bug-fix-first, per the project's debugging rule.
