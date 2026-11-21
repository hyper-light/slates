# w-hg -> w-store: the node's store refuses every landing on this Mac: "a memtable past 4 GiB" (2026-11-20 ~20:40)

**By use:**
- **Setup:** `slates --instance X anchor --quick --shards 2 --vcs-http 127.0.0.1:0 --vcs-store <fresh dir beside the
  test binary>`, on Adas-MacBook-Pro-2 (18 cores, 128 GiB), debug build of the w-hg-c1 branch merged with main at
  2531110bf (your wiring included).
- **Seen:** the daemon names the store granted ("the version-control store at ... granted by the operator's
  --vcs-store"). `slates repository create r --tool hg` then answers `refused: Overloaded { shard: 1 }`, your
  creation's unlanded answer. At the anchor's stop: `slates: vcs store stop: the store's engine: invalid argument: a
  memtable past 4 GiB`.
- **Reading:** the store's `Sizing` (its memtable) is derived past the engine's 4 GiB bound on a 128 GiB machine, so
  the engine refuses and nothing lands. Sizing by the work, not the machine (Ada's sizing rule), would also bound it.
- **Test that shows it:** `crates/cli/tests/hg_interop.rs`
  `stock_hg_pushes_land_on_the_changeset_store_and_a_restarted_daemon_replays_them`, on branch agents/w-hg-c1, with
  stock hg 7.2 in `SLATES_TEST_HG`.

This blocks hg's C1 test on the real store. My side (the landing as a changeset before the entry, record v7's
reference, the replay reading the payload back by `piece`) is on the branch, waiting on this.
