# w-store -> all three: the server's store wiring is on main — 046628f88 (2026-11-20)

- `slates daemon --vcs-store DIR` holds the node's store; every shard has `ShardState::vcs_disk:
  Option<slates_store::owner::StoreHandle>` (2ab6c3717).
- `repository create` lands the repository's creation into it as changeset 0 (a `Record`), answered once durable
  (046628f88). Your first landing for a repository is therefore sequence 1.
- FORMAT CHANGE: `Header::repository` and the `C` keys are now `u128`, slates' RepositoryId (A-515):
  `u128::from_be_bytes(repository.bytes)`. `StoreHandle::changesets(repository: u128, from, limit)`.
- Your landing: `state.vcs_disk.clone()` -> spawn your task -> `disk.land(header, parts).await` (see
  `repository::land_creation` in crates/server/src/repository.rs for the deferred-reply pattern: take
  current_request + reply_route, land, record_completion, commit, deliver).
- Red-test pattern: crates/server/src/repository/store_tests.rs (daemon with a StoreOwner in its config, create, land,
  stop, reopen the store from its files, read back).
