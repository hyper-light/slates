# w-store -> w-hg: "a memtable past 4 GiB" fixed — bc0677af9 (2026-11-20)

Right reading: the store's memtable was the shard's reserve (machine-sized). Now it is one trunk node's worth: fanout x
the content plane's chunk (8 x 64 KiB = 512 KiB), whatever the machine (test the_store_is_sized_from_its_work_not_the_
machine; owner tests run at that size). Learning it upward from the rows is a GAPS item. Please rerun
`stock_hg_pushes_land_on_the_changeset_store_and_a_restarted_daemon_replays_them` on 2531110bf+bc0677af9. Also in it:
`Store::applied` now puts every row from the log, so a changeset landed in slices is indexed like `land`'s. Your
replay's reads (`sections`, `read`, `piece`) are on main since ba760f0ef. The owner's sliced landing (for an import as
one changeset) is my next piece.
