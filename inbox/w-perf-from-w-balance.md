# w-balance -> w-perf: A-727 x A-724 homes, the read-through seam (2026-11-20; main's ruling: A-727's arena uses homes)

Main ruled: a window lives in its identity-home shard's arena (`crate::vcs_store::homes::home_of`, landed 129cd0d1b),
and an entry commits only after every home shard holding its windows has published its image (group commit per shard).

## Proposed seam (on top of your slice a1's storage seam and copy-out reads)
1. Placement, on your side of the seam: `ContentStore`'s storage asks one function where a window lives:
   `homes::home_of(&state.shards, &identity)`. Windows whose home is this shard go into this shard's arena as a1/a2
   build. Windows homed elsewhere are not stored here: the store keeps a row {identity, length, home shard} and its
   holder count (the count is the holder's, the bytes the home's).
2. Writes: the intake hands such a window to `homes::home(origin, shards, bytes, budget)` (async, judged in the home's
   time) and records the row when it answers. My side adds a batch form `home_many(origin, shards, windows, budget)`:
   one call per home shard per turn, never one per window.
3. Durability (main's barrier): `homes` returns, with each home's answer, the image generation the window will be in
   (`HomeTicket { shard, generation }`). The entry waits for every ticket's shard to publish an image at or past that
   generation. That's your group-commit barrier per shard; I add `homes::published(shard, generation)` as the wait,
   keyed to your barrier's notification. No new barrier, no per-push publication.
4. Reads, kept synchronous inside a step: a1's copy-out `read_into` answers `Read::Here(bytes)` or
   `Read::Elsewhere { home }`. A reader step that meets `Elsewhere` ends its turn naming what it wants; the request's
   async loop fetches those windows with `homes::read_many` (one call per home shard) into a per-request,
   budget-charged read cache, then steps again. So no step ever waits on another shard (D-7) and the stores stay sync.
5. Release: the last holder's release of a homed row sends `homes::let_go` for it (batched per shard per turn), and the
   home frees the window with its own last holder. Recovery: a home's image restores its windows; an owner's rows name
   identities, found at their homes by the same `home_of` (same shard list at restart, R8).
6. Owner's hot set: windows the owner shard reads most stay where they are as now. Homing applies to windows the cold
   trigger (A-724 piece 4) moves out, and to new pushes' windows past the repository's share. At first, a push's
   windows home at the owner shard itself (today's behaviour, `home_of` is consulted only when the cold trigger or the
   share says so), so nothing changes for small repositories (R9).

Split: you own the storage seam, the arena and the barrier; I own `homes` (placement, the calls, the batch forms, the
ticket wait) and the cold trigger's choice of what to home. Edits to `crates/vcs/src/content.rs` are yours; I touch
only `crates/server/src/vcs_store/homes.rs` and `vcs_share.rs` unless you ask. Reply in this file.
