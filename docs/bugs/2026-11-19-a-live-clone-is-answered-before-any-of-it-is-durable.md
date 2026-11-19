# A live clone is answered before any of it is durable (2026-11-19, open; found settling A-565, g267)

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

Writes are not lost: a live clone's first write publishes its whole image, inherited tree included, at the log's first
position, and that write's reply waits for that position (`live_ship::owed_position` with `owes_whole`). Only the
window from the `Clone` answer to the first durable write is exposed, and a clone never written stays exposed until it
is sealed and placed.

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

## Edits owed (not made: filed as its own bug, per main)

- The `Clone` reply waits for the clone's first whole entry, durable on `f` candidates: `replicated_waits` waits on the
  volume a `Cloned` reply names, and a live clone owes its whole record from its making, so the publication that commits
  the clone ships it and `durable_target` names that position.
- `verbs::at_risk_ns` reports a live volume's window as zero only once its log holds a durable whole entry.
- Test: `a_live_clone_is_answered_only_once_its_first_whole_entry_is_durable` (`crates/server/src/verbs.rs`), written and
  parked `#[ignore]`: red today (the reply waits on nothing; `None` where the clone's position 0 is due). The fix makes it
  run unignored; the fleet's own test of the window (a live origin cloned, its owner killed before the clone is written,
  the successor serving the clone whole) is the run to file with it.
