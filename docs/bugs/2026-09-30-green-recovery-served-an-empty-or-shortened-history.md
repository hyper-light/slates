# Green recovery served an empty or shortened history

**Date:** 2026-09-30. **Area:** `slates-server` (green recovery, §4.16, D-27). **Audit:** AUD-29-18.

## Description

`rebuild_green` replayed a green's durable origin and chain into a fresh engine at boot and at takeover, but
never failed closed:

- a corrupt origin was logged, and an **empty** green was installed;
- a corrupt chain entry stopped the replay, and the **shorter** green was installed;
- each replayed submit's outcome was ignored, as was the retention settlement's result.

The volume id then served an older or empty history, and new work could branch from it.

Red test: `a_green_whose_recovered_chain_is_corrupt_is_fenced_not_served_shorter`. After a corrupt entry
was appended and the green rebuilt, the shortened engine was installed.

## Fix

- **`replay_green`.** Every durable chain entry was an acknowledged version, so entry `n` must decode and be
  accepted at exactly version `n`, over the origin's version 0. Anything else returns a `GreenFence`:
  `OriginCorrupt`, `EntryCorrupt`, `NotAccepted`, `WrongVersion`, or, from settling retention,
  `Retention { short }`.
- **`fence_green`.**
  - No engine or retention stays installed. The green is recorded in `ShardState::fenced_greens` with its
    reason, counted (`merge.green_fenced`) and logged.
  - The durable origin and chain are left untouched, as the evidence a reviewed recovery works from.
- **`dispatch`.** Refuses any verb naming a fenced green with `ContentUnavailable`, before anything could
  answer from a missing engine, where some paths defaulted to head 0. The one verb a fenced green takes is
  its admin's `Destroy`, the reviewed release, which also clears the fence.
- **Takeover.** Its existing comparison against the adopted head refuses a fenced rebuild too.

## Tests

- `a_green_whose_recovered_chain_is_corrupt_is_fenced_not_served_shorter`.
- `a_healthy_green_replays_exactly_and_a_duplicate_or_corrupt_origin_is_fenced`:
  - a healthy chain replays to exactly the acknowledged head and identity;
  - the same entry appended twice is fenced `WrongVersion { version: 2, accepted: 1 }`;
  - a corrupt origin is fenced `OriginCorrupt`.
- The first test also destroys the fenced green as its admin and expects the fence released.

