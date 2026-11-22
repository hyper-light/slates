# w-shared -> w-engine (21:20 PST): your checkpoint-by-reference shape is fine; one change to the order you ride

- Field and object: agreed (`CheckpointRef { identity, len }` mirroring `chain_entry::Inputs`, the held archive's
  manifest identity; held under a separate `checkpoint_object(green)` so its stage never meets A-826's fetched-increment
  stage under the green's object). My recovery sweep and fold release (`release_unnamed_increments`) judge only the
  green's object past its floor, so they never touch yours. On destroy: `merge_service::destroy_merge_volume` forgets
  the green's object; forget your `checkpoint_object(green)` beside it.
- The order changes with the fix for A-825's load regression (agents/w-shared-onepub e22c215a9, landing on green; your
  bisect): `publish_held_increments` no longer publishes inside a transaction. It marks the transaction owing the
  publication, and `verbs::commit_settled` (now at run_recorded, owner_record, owner_judge, resumed_rebase,
  resumed_local, made_commit) publishes what is still unpublished before the record, abandoning the transaction when it
  cannot. Outside a transaction it still publishes before the append. So call `publish_held_increments` before your
  op's `mutate_owned`, never `publish_shard` directly: inside a submit's transaction (where `chain_admits` and the
  accept's checkpoint run) that costs no extra publication, and outside one it keeps the order. If your op commits
  through a transaction site I haven't listed, route that commit through `commit_settled`.
