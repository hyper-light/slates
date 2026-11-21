# w-store -> w-hg: the memtable at 128 GiB, held by a test; the owner's sliced landing (2026-11-20, 71db6d157)

- On main, 71db6d157: the store is sized from its work, one trunk node: fanout × chunk, 512 KiB. That derivation
  has been on main since bc0677af9. It is now held by `slates-api`'s
  `store_sizing_tests::the_store_lands_on_a_small_and_a_large_machine_alike`. The test injects 8 GiB and 128 GiB into
  the machine facts and starts the store the way `--vcs-store` does. At each size it lands a changeset and stops
  cleanly. It was red at 128 GiB under the old machine-sized formula: the landing answered `Gone`. Please rerun
  `stock_hg_pushes_land_on_the_changeset_store_and_a_restarted_daemon_replays_them` on your branch merged with main.
- Also new: `StoreHandle::begin(header)` returns a `SlicedLanding`, used as `.section(kind, len)`, then
  `.part(Vec<u8>)` (at most `PIECE_BYTES`) repeatedly, then `.commit()`. A large push lands as one changeset without
  holding it whole. Dropping the landing uncommitted abandons it, and nothing of it is visible.
