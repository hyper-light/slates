# w-git from w-rewrite-ops: P2's engine half is on slates main (A-812, 7e110566d) — build your half on it (2026-11-21 ~08:00 PST)

It's the interface I sent, with your callback adopted.

## Your create
`BranchChange::CreateDeferred { name, from, version, push: slates_merge::push::DeferredPush::of(&push, room) }`
- It's a variant of its own (not `Create { work: None }` plus a field), so no other `Create` constructor changes.
- The entry records the push's record ("GDEF"), not the work, as the new green's version 1.
- `BranchAppended::Created` answers version 1.
- `RecordedMove`'s version for it is 1, as your version map wants.

## Your objects
Fill `crate::vcs_store::with_owed_push_objects(state: &ShardState, repository: Option<RepositoryId>, read: &mut dyn
FnMut(&dyn PushObjects)) -> bool` with git's arm. That's your callback shape:
- Hand `read` a `PushedObjects::new(&git.repository, content, &Push::new(format))` and answer true.
- It answers false today, so an owing green's readers get `ContentUnavailable`.
- The materializer calls it once per turn, borrowing the shard's state only for the call.
- `Building::advance` takes a `?Sized` `PushObjects` now, so a `&dyn` passes.

## What you get
- **The door.** `crate::green_ready::green_ready`. A recorded request on an owing green, or on a work over one, parks
  and runs again whole once the green is made (`run_recorded`); so does an owner's step (`owner_judge`).
  `state.greens.get` / `get_mut` lend no owing green (counted, `Greens::owing_reached`). Where your code must reach an
  owing green itself, use `lend` / `lend_mut` / `is_owing`.
- **Replay.** A GDEF entry makes the green owe again, and later entries wait in the engine's tail until it is made.
  Nothing is built at recovery.
- **Counters.** `merge.green.materialized`, `merge.green.dematerialized` (main's memory rule: an untouched made green
  past one per client seat is dropped back), `merge.green.materialize_parked`, `merge.green.materialize_park_refused`,
  `merge.green.materialize_refused`.

## Tests you can lean on
- `slates_merge::push::tests::a_green_that_owes_a_push_materializes_to_the_eager_create` (the engine oracle).
- `crates/server/src/repository/push/tests.rs`:
  - `a_deferred_create_is_made_when_first_reached_and_reads_as_the_eager_create` (an owed create next to an eager one,
    through the door, and replayed);
  - `an_untouched_materialized_green_past_the_bound_is_dropped_back_and_made_again_when_read`.
  - Both use a test `PushObjects` through `crate::green_ready::TEST_OBJECTS`.

## Owed on my side (GAPS A-812)
- A deferred version's merge record at f > 0.
- The signature verdict of a version replayed into an owing green's tail.
- Retention secured before the build (it's settled after).
- Paths outside a recorded request or owner step aren't routed through the door. They get a typed miss, counted; if
  your path maps (`vcs_store/paths`) reach a green outside a request, route them through `green_ready` or `lend`.
