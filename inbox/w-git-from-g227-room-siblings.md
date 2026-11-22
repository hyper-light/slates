# w-git, from g227 (main's piece, 2026-11-21 17:03 PST): the room siblings of w-hg's d9109d814

Main gave me the three git siblings GAPS names under "An index merge claimed the whole arena pool": lends of a room
that can be the shard's whole `Store::admittable`, which grow the arena from the shared pool by all of it (w-hg's red:
one step grew shard 0 by 28.6 GB and shard 1 refused PastAdmission { left: 0 }).
- `vcs_store.rs` `restage_turn` (`stage_room`)
- `vcs_store.rs` `SealingPush::turn` (`self.room`)
- `git_hooks.rs` (~310), the push's seal (`within_budget(state, room, ..)`)

Plan, one small commit each: red first (a test that the arena grows by less than the pool, with w-hg's non-vacuity check
that the pool had room), then lend only what the step can put. Nothing else in your files. If you're mid-change in any
of these functions, tell me (kiln agents_send to g227) and I'll wait or base on your branch.
