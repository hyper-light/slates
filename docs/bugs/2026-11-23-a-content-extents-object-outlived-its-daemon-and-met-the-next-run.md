# A content extent's object outlived its daemon and met the next run under the same name (2026-11-23, A-98, A-99, A-848)

Found by main 01a20182's r633 on the big Studio (macOS, 23 shards), `provision_bench` at 4185474ca: the first
extent map the pool refused logged `content pool extent 1028 refused its map (first occurrence): memory: offset 524288
is misaligned or past the 2097152-byte object` (extent 260 in the second run). The earlier r578 refusals of 1 MiB
creates with 33.9 GB available were this claim ending at the refused map (fixed first in 80b09c3f6).

## Description

On macOS each extent of the content pool is a shared-memory object of its own, named from the content object's name
and the extent's id (`slates_mem::extent_handoff`), made at its shard's start or at a release (`make_extent`), which
keeps an object that exists at the length it wants, for a successor adopting it, and refuses one of another length. A
macOS shared-memory object lives until its name is removed, whoever holds it. An anchored daemon removes its anchor's
extent names when the anchor is gone (`release_the_anchors_objects`), but a daemon that created its segment in its own
process (`SegmentSource::Create`: `provision_bench`, the tests, an embedding) never did: the content object's creator
removed its own name and no extent's. `provision_bench` names its segment the same every run
(`slates-seg-provision-bench`), so a run met the extent objects an earlier one had left: extent 1028 at 524,288 bytes
(its part's length at a 64 KiB pool unit), where this run's layout wanted 2,097,152 (its part at 256 KiB).

## Impact

- A claim of such an extent was refused, and before 80b09c3f6 its whole growth with it.
- An extent left at the length a run wants was adopted with the dead run's bytes: a fresh daemon's arena takes a new
  region as zeros (A-99's zero-on-free assumes it), so those bytes could reach an admission's unwritten pages. No run
  is known to have read one; the lengths differed in r633.
- Every in-process run left its extent objects in the kernel with every page its shards wrote, until reboot.

## Edits

- `AnchorSegment::with_content` removes every extent name of the content object it creates before any shard makes one
  (a fresh content object has no extents), and a segment that created its content object removes them when it drops;
  an adopted content object (`adopt_content`) leaves them to its creator.
- `provision_bench` names its segment for its process.
- Red first: `slates-anchor`'s `tests/content_extents.rs`, both refused `OutOfRange { offset: 524288, len: 2097152 }`
  before, r633's refusal: `a_fresh_content_object_clears_a_dead_runs_extent_names` and
  `a_created_content_objects_extents_go_with_it`.
- The pool's claim keeps passing a refused map to the next candidate (80b09c3f6): an OS refusal of one extent (its
  descriptors, its memory) still should not refuse a growth the pool can serve.
