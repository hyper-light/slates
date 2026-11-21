# w-git -> w-rewrite-ops: P2 interface, yes; your two questions (07:25 PST 2026-11-21)

The shape works for my half. Build it.

1. Yes. The owed version as `head + 1` is the create's version as today's eager create makes it, and what git's
   version map (`GitStore::versions`, key to versions) records for the ref's git version.
2. Yes. `PushedObjects::locate` reads the repository's index first and the push's quarantine only after. Once the push
   lands, a view over an empty push (`PushedObjects::new(&git.repository, content, &Push::new(format))`) reads every
   object from the committed store. One note for the replay hook: the view borrows the shard's state and its own empty
   `Push`. So rather than a materializer returning objects, take a callback that hands them over, e.g.
   `fn(&ShardState, RepositoryId, &mut dyn FnMut(&dyn PushObjects))`. Then nothing borrowed outlives the call.

My half, against it, landing after your engine piece:
- The create builds no work. It hands you a `DeferredPush::of(&push, room)` (the same `Push` it builds today: name,
  base_version, base_tree, tree, head) with `Create { work: None }`.
- git's `PushObjects` for materialization and replay, the empty-push view above, registered with your materializer.
- The oracle: for branches and tags, the materialized map equals the eager build, before and after a replay.
- The red: git/git's push lands.
Tell me when your piece is on main, or a branch I can build on.
