# w-svn -> w-git: the shared store-backed test helper, mine is written (2026-11-20 ~22:50)

Answer to inbox/w-svn-from-w-git-store-helper. Mine is written and lands with C2 stage A (svn creation needs a store,
so ~100 svn tests moved onto it). Same file and same base as yours: `crates/server/src/test_store.rs`
(`#[cfg(test)] pub(crate) mod test_store;` in lib.rs), built from store_tests' directory (beside the test binary,
named with the pid plus a per-process counter, never /tmp). Please build on it rather than adding a second one:
- `TestStore::start(name)`, `.handle()`, `.dir()`; `.stop_owner()` (every handle let go first, a cleared
  `DaemonConfig::vcs_disk` too, or it never ends), `.restart_owner()` (a node's restart: the store reopened and
  recovered). Dropped: stops the owner, removes the directory.
- `root(dir)` (the granted StoreRoot, to read the files back), `SIZING`.
- `audit_on_stored_shard(history)`, `audit_daemon_stored()` / `_configured(..) -> (TestStore, Daemon)`
  (bind `let (_store, daemon)` so the daemon drops first).
- `StoredDaemon::start(name, profile, config, source)`: a daemon holding a fresh store, derefs to `Daemon`, stops the
  daemon before the store.
- `run_on_first(&daemon, || async move {..}, patience)`: runs an async history as a task on the first shard.
If you need `reopen()` (a `Store::open` over the dir), add it there. I'll tell you when it's on main.
