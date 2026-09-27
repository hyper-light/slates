# NFSv4 `change` was the change time, and the volume's counter missed changes

**Date:** 2026-09-26. **Found:** auditing the NFSv4 front end's attribute encoding (`v4/attr.rs`
`change_of`) against RFC 8881 §5.8.1.4 while triaging pjdfstest over NFSv4.2.

## Description

- NFSv4's `change` attribute was `ctime` in nanoseconds.
- `Volume::change_version` promised a counter that "increases on every change to the object", but
  `adjust_nlink` and `drop_link` (a hard link, an unlink of one of two names, a rename over a name)
  stamped ctime without moving it, and the base plane's refreshes of an outsider's edit
  (`adopt_fingerprint`, `follow_live_disk`, the copy-up witness) did the same.
- Every `change_info4` was non-atomic 0/0, and NFSv3 `wcc_data` carried no pre-operation attributes.

## Root cause

The v4 front end reads attributes through the v3 semantic layer, whose `fattr3` has no field for a
change counter, so the change time stood in. The counter's rule (move with every ctime stamp) was held
by habit at each call site, and two call-site families were written without it.

## Impact

- The wall clock repeats stamps within its resolution (a microsecond on macOS) and can step back. Two
  changes inside one tick left `change` unmoved, so a v4 client kept its cached data, attributes or
  listing across the second change: stale reads. A clock step back made `change` go backwards.
- A client's own create or remove always dropped its cached directory listing (0/0 non-atomic, no
  pre-op attributes), a revalidation per namespace change.

## Exact edits

- `slates-vfs`: `Inode::stamp_change` (ctime and counter together) at every ctime site;
  `adopt_observed` for host-observed attributes (moves the counter only when they differ);
  `fold_counter` and `view_change` for AppleDouble views; `Volume::observe`.
- `slates-bridge-core`: `NodeAttr::change`.
- `slates-bridge-nfs`: the per-call `Dialect` (replacing the AppleDouble toggle), the change-carrying
  `Fattr3`/`WccAttr`, `Wcc` with pre-operation attributes in every mutating procedure, `change` served
  from the counter, atomic `change_info4` from one call's wcc, the pseudo-root's listing digest.
- `slates-server`: the requester's dialect.
- Tests first: `crates/vfs/tests/change.rs` (failed on a hard link), the base test
  `an_outsiders_edit_moves_the_change_counter_and_a_plain_stat_does_not` (failed), and the v4 tests
  `change_moves_on_every_write_under_a_frozen_clock` and
  `create_and_remove_answer_atomic_change_info_under_a_frozen_clock`.

Sibling check: every `attrs.ctime =` in `slates-vfs` now goes through `stamp_change` or
`adopt_observed`, except creation (`stamp_all`) and a new base inode's first attributes. The
pseudo-root's cookie verifier had the same flaw (the volume count repeats after a remove and an add);
fixed in the same change.
