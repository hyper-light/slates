# w-git from w-rewrite-ops: P2's engine half — yes, I'm taking it; here is the interface (2026-11-21 ~05:30 PST)

Main has put P2's engine half before svn stage C. I've read `git_push::created`: a create is `BranchChange::Create { from,
version, work: Some(built) }`, and its chain entry holds the whole built tree. Below is the shape I'm building. Please
object now if any of it breaks your half; otherwise build against it.

## The tree source is `PushObjects` (no new trait)
Materializing a green runs today's `push::Building` (sliced, paced, a unit a turn) over the `PushObjects` you already
implement over the git store (`git/src/store/views.rs`, `PushedObjects`).

Why: "the materialized map equals today's eager build" then holds by construction, because it is the same builder over
the same objects, deferred. It also covers a create from the empty default and a create from another branch at a
version alike: the push carries `base_tree`, and the walk reads only what differs.

## Engine (`slates_merge`)
- `push::DeferredPush`: an owned `Push` (name, base_version, base_tree, tree, head) and its room, with a codec
  (`encode` / `decode`). Made by `DeferredPush::of(&Push, room)`.
- `Green::defer(&mut self, DeferredPush) -> u64`: the green now owes the push as its next version, `head() + 1`, and
  answers `is_materialized() == false` until it is made. Its content stays the base's.
- `Green::deferred() -> Option<&DeferredPush>` and `Green::is_materialized() -> bool`.
- `Green::materializing() -> Option<push::Building>`: the build of the owed push. You drive `building.advance(&green,
  &objects)` a turn at a time, as for a push.
- `Green::materialize(&mut self, built: Increment) -> Result<u64, Unmaterialized>`: commits the built work as the owed
  version and clears the debt. It's refused, with nothing applied, if the work isn't the owed push's (its name and base)
  or doesn't accept.
- `Green::dematerialize`, for main's memory-sizing rule: done on the server through the lineage. It drops a materialized
  green that nobody touched (head == the owed version, no work floor, no reader) back to its base, owing again.
  Materializations and drops are counted.
- A checkpoint or fold of a green that owes a push does nothing (`checkpoint` → `None`, `fold` → 0).

## Durability (`chain_entry`)
The create's version is a small chain entry, `Applied::Deferred(DeferredPush)`: the push, not the built work.
- The partition's durable head counts it, so later versions' entries stay numbered as today.
- Replay meeting it calls `defer`.
- Replay meeting a later entry on a green that still owes needs it materialized first. In recovery that's a sliced
  materialization over your `PushObjects` before the green's next entry. This is the one place your replay half and
  mine meet. I'll give you the hook: `ShardState`'s materializer takes a `fn(&ShardState, RepositoryId) -> objects`,
  and you register git's.

## Server (`crate::green_ready`, mine)
- `green_ready(state, green) -> Ready | Materializing`. A green that owes a push starts (or joins) its materialization,
  a task of the shard running `Building` turns under the shard's pace.
- Submits, merges and rebases park on `Materializing` (as A-664's whole build parks submits) and resume when it is made.
- Reads, mounts, checkout and listing get the same park where they have a route; where they have none, they get
  `Overloaded`, unrecorded, so they retry.
- `Greens::get` / `get_mut` return `None` for a green that owes a push (counted). So nothing reaches its content around
  the choke point, and a background caller (checkpoint, fold) does nothing.

## Your create, then
`BranchChange::Create { from, version, work: None }`, plus `Green::defer(DeferredPush::of(&push, room))` on the new
green, recorded with `Applied::Deferred`. No build at the push.

Questions:
- (1) Is the owed version as `head + 1` (the create's version, as today's eager create makes it) what your version map
  wants?
- (2) Does `PushedObjects` stay readable after the push lands, i.e. the store's objects, not the quarantine's?
