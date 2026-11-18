# A recovering green seeded its whole origin in one step (2026-11-18, §4.8 "Recovery", §4.3 "Bounded work everywhere")

Found by reading, for goal g232 (100,000 clients against one primary and two replicas editing the Linux kernel tree):
the load launcher's chaos mode kills the primary's daemon and lets its anchor restart it (`SLATES_LOADTEST_KILL_NODE`),
so the owner's own restart over the whole kernel is on the goal's path. A-217's amendment kept the item owed:
"Recovery walks a held origin in `init_shard` in one call."

## Description

A restart rebuilds each green from what its partition records (`verbs::rebuild_green_sliced`, through `GreenReplay`):
its origin, then its checkpoint, then the chain entries after it. Since recovery was sliced (2026-11-07) a green's held
origin is walked a directory a unit, and its chain replayed an entry a unit. Between the two, the engine was built from
the whole origin in one step: `Green::with_origin`, or `Green::restore` over it when the green has a checkpoint. A
taken-over green's origin, recorded as bytes, was also decoded whole in one step first (`Origin::decode`).

Seeding hashes every file's bytes into the head identity and copies them into the engine's content and history
(`slates_merge::engine::OriginLoad::move_one`). Over the kernel (97,229 files, 1.67 GB) that is seconds of one step. The
watchdog judges a shard that runs a window of CPU in one step stuck (A-232, 1,000 ms), so the restarted owner was ended
inside its recovery, and its anchor's next start met the same step.

## Measured

The failing tests:
- `a_recovering_green_decodes_and_seeds_its_origin_a_share_at_a_time` (`crates/server/src/verbs.rs`, unit): a green
  recording an origin of four files of a share each, replayed a unit at a time.
- `a_restarted_owner_seeds_a_greens_origin_a_share_at_a_time` (`crates/server/tests/recovery.rs`): a green over a pinned
  overlay's snapshot of four files of a hashing share each, the daemon restarted over its segment.

Run r1017 (kiln, 2026-11-18, the Mac Studio `d1323898`, debug build, a clone of
`agents/g232-100-000-clients-recovery-load` at 9ac30eb67; logs `~/.cache/g232-logs/recovery-load-2/` there):
- **Red** (in a copy, the decode and the seeding each given an unbounded share, so each is one step, as before): the
  unit test failed "the origin decoded over 1 units", and the recovery test failed "the recovery seeded the origin over
  1 shares" (`merge.recovery.origin_load_steps` 1).
- **Green**: the unit test passed in 0.75 s and the recovery test in 1.90 s, each seeing more than one unit; slates-merge's
  checkpoint tests passed 4 of 4, the new property (`a_green_seeded_in_shares_then_given_its_checkpoint_is_the_restored_green`)
  among them.

## Root cause

Recovery was sliced unit by unit, and the engine's seeding was taken as one unit. The green's creation had been sliced
the same way a day earlier (A-217, "Created in slices", 2026-11-07: `OriginLoad`), but recovery kept the one-call
constructor, so nothing tied the two paths to the same bound.

## Fix

`GreenReplay` (`crates/server/src/verbs.rs`) gains two phases between the origin and the chain:
- `Decode`: a taken-over green's recorded origin bytes decoded a share at a time (`slates_merge::origin::DecodeCursor`,
  each share the shard's copy slice), re-reading the bytes from the partition at each step, so no borrow is held;
- `Load`: the engine seeded from the origin a share at a time (`OriginLoad`, each share the shard's hashing slice).

Then the checkpoint is applied onto the seeded engine (`Green::with_checkpoint`, new in `slates-merge`; `Green::restore`
is now the seeding and that, so both paths restore the same green). The recovery counts the seeding's shares
(`merge.recovery.origin_load_steps`). The unsliced `replay_green` (a takeover's materialization, tests) steps the same
phases to their end.

## Impact

Every restart of an owner whose green's origin seeded past one window of CPU ended inside its own recovery, and every
start after it did the same: "Green owner restart: Masked" (§4.16's failure matrix) did not hold for a kernel-sized
green, and a primary kill in the launcher's chaos mode would have taken the green away for good.

## Siblings (swept)

- **The checkpoint applied onto the seeded engine is still one step.** It costs what the green's versions changed since
  its origin (each changed file's whole value at the checkpoint): bounded by the green's history, not its origin, and
  recorded in GAPS.
- **A takeover's materialization of a green is one step on each side** (`merge_service::recover_green_inputs`,
  `verbs::materialize_taken_over_green`): the dense gather of every input, the origin recorded as bytes (refused
  `Capacity { table: "green_chains" }` past the partition's chain cap) and the chain replayed twice. Open in GAPS (A-217,
  A-238 and A-286's owed items); goal g232's next piece.
- **A taken-over plain volume is restored in one call** (`verbs::materialize_taken_over`: `slates_archive::restore`, then
  every file written): over a kernel-sized base volume, the same wall on the volume's successor. Recorded in GAPS.
