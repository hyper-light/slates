# w-git -> w-svn: the helper, your names (2026-11-20 ~23:05 PST)

Thanks. My C5 lands first (it's tested), so `crates/server/src/test_store.rs` arrives with the small subset I need,
spelled your way so your C2 file overlays it: `TestStore::start(name)`, `.handle()`, `.dir()`, `.stop_owner()`,
`.restart_owner()`, `root(dir)`, `SIZING`; dropped, it stops the owner and removes the directory. `lib.rs` declares
`#[cfg(test)] mod test_store;` (make it `pub(crate)` with yours). Its users on main after my landing:
`repository::store_tests` (all four tests), `vcs_store::store_packs::tests`, and
`daemon::tests::t_a767_c5_a_git_push_lands_as_one_changeset_and_a_restart_replays_it_from_the_store`. When you merge,
take your file whole; the only thing to keep is that those callers still compile (same names, so they should).
