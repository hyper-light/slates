# 2026-09-05: a merged directory's link count ignored its base subdirectories

**Found by.** The landing oracle (`crates/land/tests/oracle.rs`, the crash scenario), while
removing a nested base tree through the overlay: `rm -r /tree` where `/tree` held a file and a
subdirectory on the base.

**Description.** `Overlay::rmdir(root, "tree")` refused `NotFound` after `x.txt` and `y` had
been removed beneath it, and left a whiteout for `tree` behind (the removal had run half way).

**Root cause.** A base directory materialized on first touch (`Overlay::materialize`, the
`HostKind::Dir` arm) received a link count of two, whatever it held. Removing its base
subdirectory `y` decremented the count to one (`Volume::rmdir` → `adjust_nlink(parent, -1)`,
correct for POSIX). Removing `tree` itself then dropped two links (`drop_link` twice, the
directory's own): the first took the count to zero and retired the inode from the table; the
second looked the inode up and refused `NotFound`. The whiteout had already been recorded by
`dir_remove`, so the refusal came after the mutation.

**Impact.** Any overlay removal of a base directory after one of its base subdirectories had
been removed; a scratch volume or a base directory without subdirectories was unaffected. The
base oracle (T-1.12) removed a directory of files only and did not see it.

**Fix.** `Overlay::refresh_dir_nlink` (`crates/vfs/src/base.rs`), called at the end of
`load_listing`: a merged directory's count is two plus its subdirectories, overlay and base
alike, once its listing is known; the core keeps it in step from then on. `Overlay::stat` of a
merged directory loads the listing first so the count it reports is right. The constant
`ROOT_LINKS` names the two.

**Siblings checked.** `materialize`'s file and symlink arms carry a count of one (correct);
`mkdir` over a whiteout and `rename` of a base directory keep the core's adjustments; the crash
scenario now removes a nested base tree through the overlay at every crash point.
