# A live clone was answered before any of it was durable (2026-11-19, found settling A-565, fixed by A-577; g267)

## Description

A clone of a live-shipped volume is live-shipped too (its record starts as a copy of its origin's,
`verbs::publish_clone_as`, so it inherits `policy.live_shipped`). Live shipping promises that a verb on a live volume
answers only once what it published is durable on `f` candidates (A-200, §4.10), and the volume's status reports a zero
loss window for it. The `Clone` verb breaks the promise: it answers `Cloned` before anything of the clone is held by
another member. At `f = 1`, an owner lost between that answer and the clone's first write durable at its holders leaves
the successor with no log of the clone (`live_ship::fetch` answers `Ok(None)`: nothing was ever shipped) and no seal of
it (a fresh clone's head is its origin's until its first snapshot), so it serves the clone as an empty tree, or not at
all when the clone's catalog had not been placed either. The client was told the clone exists, holding its origin's
snapshot.

Corrected while fixing it (A-577): the clone's writes were not safe either. A clone's image lists only the inodes it
made since it was cloned and names the rest, everything it shares with its origin snapshot, by number
(`VolumeImage::origin_shared`), which recovery restores from the origin beside it. A live clone's whole records were
that image, so its log held no inherited tree, not even its root: a successor's rebuild found no root to lay out
(`fleet.live.takeover_refused.layout`), or, once the clone had changed its root, a tree missing every untouched
inherited file. The settlement written for A-565 ("its first write publishes its whole image, inherited tree included")
was wrong.

## Root cause

A recorded verb's live wait is chosen by `verbs::replicated_waits` for the verb's own volume, `RequestBody::volume()`.
For `Clone` that is the origin, whose log the clone did not change, so the reply waits for nothing; the clone, which
has no shipper until its first publication captures it, is never consulted. `live_ship::durable_target` also answers
`None` for a live volume with no shipper, so even a wait on the clone would find nothing to wait for until something
publishes it.

## Impact

Any client of a live-shipped volume that clones it (a workspace made from a live base, a green's work) can be told the
clone exists and lose it, content and all, to one owner loss at `f = 1`; the loss report says nothing was at risk. Kiln's
fleet workspaces are not affected: kiln never turns live shipping on, and a volume that is not live-shipped reports its
whole age at risk until its first seal is placed (`verbs::at_risk_ns`), which is the promise it is made with.

## Edits (A-577)

- `verbs::publish_clone_as`: a clone's registers start at the creation head (head none, sequence 0), with no takeover
  record and no live holders of its origin's, so the record plane ships its catalog and creation head at once and the
  fleet knows it. Copied from its origin, its head named the origin's snapshot at the origin's sequence, and nothing of
  the clone shipped until its own first seal: at `f > 0` its log could never answer (`live_ship::head_placed`), so even
  its writes waited until the held-reply bound refused them, and a takeover never found it.
- `verbs::replicated_waits`: a `Cloned` reply waits on the clone's own log, its first whole entry, which the Clone's own
  publication appends (a clone never published is not clean, so its whole image is captured).
- `verbs::clone_named_elsewhere` (a clone named on another partition, A-236): its outcome committed with the request kept
  in flight (`image::commit_in_flight`), its alias settled, then its reply delivered or held for the same durability
  (`image::answer_in_flight`), so the alias is still confirmed before the reply.
- `Volume::to_standalone_image` (`crates/vfs/src/recover.rs`): a volume's image with every inode its head reaches,
  sharing nothing and naming no origin; `live_ship::whole_record` builds every whole record and install image from it,
  so a live clone's log holds its whole tree, content and all. A clone's later full records (after it diverges an
  inherited inode) still name its shared inodes by number; a rebuild keeps those, with their content runs, from the
  image before it (`live_ship::keep_shared`), refused `fleet.live.takeover_refused.shared` when that image lacks one.
- `live_ship`: `LiveVolume::nothing_answered` marks a log begun new on its owner until it answers at a whole entry
  (cleared for good in `resolve`); `holds_nothing_yet` answers that, or, for a live volume with no shipper, whether
  nothing of it was ever shipped; `verbs::at_risk_ns` reports a zero window only once it is false.
- Tests: `a_live_clone_is_answered_only_once_its_first_whole_entry_is_durable` (`crates/server/src/verbs.rs`, the parked
  test, un-ignored), `a_fresh_clone_ships_its_creation_head_at_once` (`crates/server/src/fleet.rs`),
  `a_live_clones_reply_waits_for_one_acknowledged_whole_image` and
  `a_clones_full_record_keeps_what_it_shares_from_the_image_before_it` (`crates/server/src/live_ship.rs`), and
  `a_clones_standalone_image_holds_every_inode_its_head_reaches_and_shares_none` (`crates/vfs/src/captured.rs`); each red
  with its rule taken out, green with it.
- Not changed: an image merged as a clone (`image::run_merge`) still answers before its live clone's log holds it, as
  this path did (filed in GAPS); the fleet's own test of the window (a live origin cloned, its owner killed before the
  clone is written, the successor serving it whole) is filed for main.
