# A taken-over FUSE mount took a replayed write's bytes as fresh (A-567, goal 85 B8)

**Found:** the goal-83 audit's B8 (GAPS, "A dirty page from before a takeover is judged fresh", open since A-158). Not
reproduced on a live mount; shown by a unit test of the adoption rule.

## Description

A FUSE mount survives its daemon's death (A-61): the anchor holds its device and a successor serves it again with a
restored serve state. The successor replays every write its predecessor acknowledged after the last publication from
the write log (A-63) before the mount's first turn. The restored mount then tracked the byte states of each file its
kernel held open (the restored opens at the first turn, A-152; any other handle when a request first named it, A-147)
as fresh as of its start: the journal's head at that first turn, after the replay. So a page the kernel had cached
before the takeover, dirtied through a shared map, was judged fresh over the replayed write's bytes and applied whole
when written back. The takeover round's invalidation (A-158) launders such a page at once, which is when it arrives.

## Root cause

`crates/bridge-fuse/src/channel.rs`: `adopt_restored` adopted each restored open with `adopted(ino, start, Some(&[]))`,
and `adopt_unseen_handle` with the journal's changes since `started_at`. Both points are the head at the mount's first
turn. The successor's journal began again at its start, so the replayed writes are records before that head, and no
adoption saw them.

## Impact

A store through a shared map of a file whose bytes another path changed after the kernel fetched them, and whose change
reached the successor only through the write-log replay, overwrote that change when its page was written back after the
takeover. A write the dead daemon acknowledged was undone by a write-back the successor took as fresh.

## Fix (exact edits)

- `crates/bridge-fuse/src/coherence.rs`: `TAKEN_OVER_FROM` (0, before the successor's first journal record, where A-231
  places a held NFS connection's carried states), `TakenOver`, `taken_over` (fresh as of `TAKEN_OVER_FROM`, with every
  change the journal holds since applied, each the mount's own where its own records took it) and `changes_since`; the
  rule is cfg-free, so its test runs on every host.
- `crates/bridge-fuse/src/channel.rs`: `adopt_restored` and `adopt_unseen_handle` adopt through `taken_over`, the room
  for each file's ranges made first (`TakenOver::most_ranges`); `pending_changes` reads through `changes_since`.
- Test: `a_replayed_write_leaves_its_bytes_stale_in_a_taken_over_mounts_states` (`coherence.rs`): the bytes written
  before the mount's start must be stale in the adopted file's states, and the rest fresh. Red before the fix
  (`Some([])`, Mac Studio r138, 2026-11-19). The fix's first run (r258) failed on the test's premise: its write at
  16..48 into an empty file is recorded as an extension from byte 0, so the test now writes 0..32 and expects those.

## Left, stated

- The write log names no writer, so the mount's own replayed write is taken as another path's: a store through a map
  over exactly those bytes before a re-fetch is refused once (`EIO`), the page then invalidated and re-fetched.
- A change before the last publication whose invalidation the dead daemon never delivered is in no journal of the
  successor, so it is not marked: carrying a FUSE mount's byte states in each publication (A-231's pattern) would end it.
