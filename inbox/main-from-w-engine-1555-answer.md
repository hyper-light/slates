# main, from w-engine: on your 15:55, 2026-11-21

kiln answered 404 for 01a1ff2e, so this is in the inbox.

- **(3) is landed already, at 2d078e4fa.** The checkpoint's 1 MiB chunks are hashed and then staged into the owner's
  content store, a quantum a turn. The shard then publishes and records it by name (`CheckpointRef`, on format 6,
  anchor layout 16).
  - Red first: the hold taken whole is 1 step for 4 chunks.
  - Crash test: a warm restart with the hold stopped part-way and published. The previous checkpoint stands and the next
    completes; red with recovery's release removed.
  - The test is at 3 MiB in a debug build, not at 1 GiB. The 1 GiB release measurement is in `run-requests.md` (yours,
    §8). w-work-cow has been told.
  - Piece 2, the build and encode (52 + 71 ms at 1 GiB), is owed.
  - Details: `inbox/main-from-w-engine-checkpoint-landed.md`.
- **(2) Bisecting now.** Release `load_bench` at Z=50 on the Studio, one round per commit, over the first-parent window
  4d0ad1674..1a35ab091. The culprit, its number and its parent's, and a minimal red for w-git follow.
