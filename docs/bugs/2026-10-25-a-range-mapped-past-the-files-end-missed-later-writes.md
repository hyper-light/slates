# A range a guest maps past a file's end misses later writes into it (A-163)

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record.
- **Subsystem:** the backed DAX session's fill, `covered` in `crates/bridge-core/src/dax_backing.rs`.
- **Severity:** correctness, silent wrong data. A guest that maps a range past the file's end goes on reading zeros
  there after another path writes into that range.
- **Exposure:** the Shards route (A-163), whenever a guest's idea of a file's size is larger than the volume's: after
  another path cut the file, and before the guest learns the new size. Slates' own virtio-fs device (goal G-vhost)
  uses the same session.
- **Found by:** reading the session while building the device's backed windows; confirmed by a failing test.

## Symptom

`crates/bridge-core/tests/dax_backing.rs`, `a_range_asked_past_the_files_end_takes_a_later_write_into_it`, before the
fix:

```
the extending write did not reach the object over the range the guest maps past the old end
```

## Root cause

- A Linux guest maps the 2 MiB range that holds the offset it touches, by its own idea of the file's size
  (`fs/fuse/dax.c`, `fuse_iomap_begin`, which declines only at `pos >= i_size_read(inode)`). After another path cuts the
  file, that size is stale until the guest's attributes are revalidated, so the guest can map a range wholly past the
  volume's end.
- `covered` clipped an asked range to the file's size, so such a range was never marked filled. Write-through reaches
  only filled ranges (`ShardsBackings::write_through`), so a later write into the range, extending the file again, never
  reached the object the guest maps. The guest never asks again for a range it holds (it keeps its own tree of mapped
  ranges), so it read zeros there for as long as it held the mapping.

## Fix

- `covered` covers every range an asked range touches, past the file's end too. The fill copies nothing past the end
  (`fill_slice` stops at the size), so such a range costs its charge and no copy, and from then on it takes every
  change like any other.
- A whole-file ask (no range) still covers the file.
- Tests: the failing test above, now green; the unit test of `covered`, renamed
  `a_fill_covers_every_range_an_ask_touches_and_a_whole_file_ask_the_file`, now states the rule. The 25 Shards tests
  pass unchanged.

## Notes

- A range past the end counts against the fill budget like any filled range. That is the right charge: a guest can
  store into every page it maps, and those pages are the object's memory.
- Observed while reading, not changed: a fill refused part-way leaves the pages it stored in the object while
  unmarking the range. A later fill of that range skips what the volume then holds as holes or zeros, so those stale
  pages could survive into a guest's map. It needs the OS to refuse a store mid-fill. Recorded in GAPS, owed a test
  with an injected refusal.
