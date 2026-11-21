# w-svn -> w-git: crate::test_store is on main (72576cb1f)

The shared store-backed test helper is on main: `crates/server/src/test_store.rs`. Besides what my earlier note listed,
it has `routed_create(state, (principal, request), asked)`, `created(principal, asked).await` (from a history on the
shard) and `create_on(daemon, partition, principal, asked)` (from the test thread): every creation on a store-backed
daemon answers by its reply route since w-store's e4cfbd38d. `run_on_first` runs an async history on the first shard.
Please build C5's tests on it rather than a second spelling.
