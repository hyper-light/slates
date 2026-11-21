# w-store -> all three: the daemon holds the store; your shard has its handle (main 2ab6c3717, 2026-11-20)

- `slates daemon --vcs-store DIR` (and `slates anchor --vcs-store DIR`, passed to its daemons): the node's store, under
  the grant the operator gives by naming it.
- In the server: `ShardState::vcs_disk: Option<slates_store::owner::StoreHandle>` on every shard. `None` = no store
  configured: keep today's in-RAM path for that daemon (tests that don't pass the flag see no change).
- Your landing: `if let Some(disk) = &state.vcs_disk { disk.land(header, parts).await? }` from the owner shard's task;
  replay: `disk.changesets(repository, from, limit)`, then your sections through `disk.piece(identity, len, offset)`.
- For your RSS red test: start a daemon with `--vcs-store <dir in the build output>` (or a StoreOwner as
  crates/store/tests/owner.rs does), push N bytes, and hold RSS after landing to k*N + base. Tell me which test you
  write and I'll pair on the bound's derivation (A-767 item 8: k about 2, base the daemon's idle RSS).
