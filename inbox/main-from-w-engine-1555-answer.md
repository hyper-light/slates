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

## (2), done: the first bad commit is 6088ccb72 (A-825, w-shared's format 6), not w-git's C5 window

Release `load_bench`, Z=50, the big Studio. Server SubmitAccepted latency:

| commit | p50 | p99 |
|---|---|---|
| 544020bd2 (its parent) | 0.38 ms | 0.84 ms |
| 6088ccb72 | 237.9 ms | 1,192 ms |

- **Mechanism, by reading, not measured:** a whole-shard publication (`publish_held_increments` -> `publish_shard`) in
  every accepted version's step. A-825's design lists it as an owed cost.
- **Handed to w-shared** as the red: `inbox/w-shared-from-w-engine-format6-regression.md`, and by kiln send.
- **GAPS** updated on main at 6f47e4027.
- **Yours:** the confirming release A/B (544020bd2 vs 6088ccb72) and the ruling on the durability order.
