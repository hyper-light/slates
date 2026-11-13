# A listed volume recovery could not rebuild answered NotFound

**Found** 2026-11-06 by A-227's Windows reacquire test (`a_base_a_held_drive_covers_is_not_reacquired_through_it`,
diag-slabs-13, run 42170240452): the covered volume, which recovery refused, answered `status` with `NotFound`.

**Description.** A volume whose catalog record survives a restart but which recovery cannot rebuild (its base is gone,
or lies on the daemon's own mount, or its image is refused) is still in `volume list`: the list reads the catalog. But
every verb on it found no live slot and answered `NotFound`, as if it did not exist. Ada's rule: a listed volume answers
its real state.

**Root cause.** `verbs::find` looked the volume up only among the shard's live slots (`by_id`) and refused anything
absent there `NotFound` (`absent`). `rebuild_recovered` logged why a volume was not rebuilt, as a string, and kept
nothing of it.

**Impact.** A user, an SDK or the CLI could not tell a volume recovery refused from one that never existed. Windows
`slates unmount`, which asks every volume's status, stopped at it (the sibling, fixed in its own change).

**The edits.**
- `crates/ipc/src/protocol.rs`: `Refusal::NotRecovered { cause }` and the closed `RecoveryCause` (no image, blocks not
  claimed, base missing, base on the daemon's own mount, base refused by the host, a clone's origin not recovered, the
  image refused, resources unavailable), both appended.
- `crates/server/src/verbs.rs`: `rebuild_volume` and `build_recovered_volume` return the typed cause;
  `rebuild_recovered` keeps it in `ShardState::unrecovered`; `find` answers it for a listed volume with no slot.
- The test: `a_volume_recovery_could_not_rebuild_is_listed_and_its_status_names_why` (`crates/cli/tests/cli.rs`):
  red on macOS before the fix (`slates: refused: NotFound`), green after.

**Siblings found, not fixed here.** `destroy` goes through `find` too, so such a volume cannot be destroyed: it answered
`NotFound` before and answers `NotRecovered` now. The catalog record stays until a later change lets a destroy remove a
record with no live slot (GAPS). **Fixed 2026-11-08:**
`2026-11-08-a-volume-recovery-could-not-rebuild-could-not-be-destroyed.md`.
