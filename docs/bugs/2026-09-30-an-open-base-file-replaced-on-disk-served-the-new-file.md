# An open base file replaced on disk served the new file

**Date:** 2026-09-30. **Area:** `slates-vfs` (overlay, §4.5), `slates-bridge-core` (§4.6 "Base files").
**Found by:** AUD-29-16's sibling sweep
(`docs/bugs/2026-09-30-a-rename-replaced-a-base-directory-it-had-not-listed.md`, "Open, reported").

## Description

An outsider replaced an untouched base file that a mount held open, with a new file at the same name (a
save-by-rename). The next relist of its directory rebound the open inode to the new file: same inode
number, the new file's fingerprint, and a descriptor on the new file. The open handle then read bytes it
had never opened. POSIX keeps an open file the file it opened.

Red test: `crates/bridge-core/tests/base_overlay.rs`,
`an_open_base_file_replaced_on_disk_keeps_serving_the_opener_what_it_opened`. The name still mapped to the
opened inode.

## Root cause

A relist refreshed every untouched entry (`refresh_unloaded`) whatever held it. The volume tracked
references, but a reference also covers a transport's lookup, so it can't say "open". Nothing recorded
opens.

## Fix

- **Opens are recorded.** `Volume::open_for` / `close_for` count open handles per inode, overall and per
  attachment. `sweep_attachment` releases a lost mount's opens with its references. The bridge pins at
  `open`, after the base descriptor is taken, and unpins at `release`.
- **An open inode is detached on replacement.** When a relist lists a name whose `(dev, ino)` is not the
  file an open inode's held descriptor names (`names_another_file`), the inode is detached from the name
  (`drop_unloaded`). It lives on, held by its references and its descriptor. The name materializes the
  new file as a new inode on its next lookup. An inode no handle holds open is refreshed as before, so the
  live disk still shows through.

## Tests

- The red test above now passes. The name serves the new file under a new inode number, the open handle
  reads the old bytes, and the old file is reclaimed once released.
- `a_teardown_sweep_releases_the_opens_a_lost_mount_held`: a swept attachment's open no longer holds the
  replaced file.
