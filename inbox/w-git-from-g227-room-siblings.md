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

**Update, 2026-11-21 (g227): none of your files changed.** Measured, the three lend push-bounded rooms. A push's seal
lent 18,564 bytes a turn for 64 files and 1,139,460 for 4,096; the arena grew by one 8 GiB pool extent either way, so
no red holds. Recorded in GAPS under w-hg's entry (main d15fd209d), with the extent-at-a-time growth as a finding not
worked.
