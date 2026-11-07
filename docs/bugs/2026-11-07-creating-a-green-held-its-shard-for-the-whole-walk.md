# Creating a green held its shard for the whole walk of its base

**Found** 2026-11-06 by loadtest-scale. With the whole Linux v7.4-rc1 tree as the load test's base (97,229 files,
1.67 GB, in-process daemon on a Mac), `CreateGreen` ran as one shard poll of 69,028 ms, and RSS went from 3.4 to
17.8 GB, holding 10 GB after (GAPS, "The whole kernel as the workload"). An anchored daemon is killed long before 69 s
(the liveness budget is 1 s), and every verb on the shard waited for the whole walk.

**Red, reduced** (`crates/server/tests/green_fairness.rs`, `creating_a_green_over_a_large_base_never_holds_its_shard`,
1,000 files of 16 KiB in 20 directories, one shard, a second client probing it). CI diag run 42371545928, main
`4326a9d8` with the test, 3 of 3 red on each runner: one poll of 134 ms on ubuntu-latest and 960 ms on
ubuntu-24.04-arm, 0 probes answered inside it.

**Root cause.** `create_green` called `seed_origin` → `walk_origin` in the verb itself, so the whole walk ran in one
poll:
- every file's bytes were copied out of the snapshot into the `Origin`;
- `origin_identity` hashed `origin.encode()`, which cloned the whole origin to canonicalize it and then wrote a second
  whole copy into a buffer, only to hash it;
- `Green::with_origin(&origin)` cloned the origin again to canonicalize it, then hashed every file into the head
  identity and filled its maps.
Canonicalizing sorted each table with a key closure that cloned the path for every comparison.

**Fix.**
- `CreateGreen { base }` checks at once (the read right, the snapshot, its completeness) and defers its reply to a task
  of the owner shard, which steps through `image::sliced` as a pin and a checkout do. Three stages:
  - the walk (`merge_service::OriginWalk`): one directory listing or one file read a unit, no handle held between
    units, in the one-call walk's order, so a held origin's identity is unchanged;
  - the identity (`slates_merge::origin::IdentityCursor`): the canonical bytes hashed a share at a time, never gathered
    into a buffer, held equal to `blake3(encode())` and to a golden vector;
  - the seeding (`slates_merge::engine::OriginLoad`): the origin's entries moved into the engine a share at a time, and
    `Green::with_origin` takes the origin by value.
- The green's records (`VolumeCreated`, `GreenOriginHeld`) commit with the request's completion as one atom
  (`found_green` inside `image::complete`).
- A name taken while the walk ran is refused `AlreadyExists` at the end. A base destroyed meanwhile is refused
  `NotFound`. A scratch green is still answered at once.
- `encode()` and `identity()` copy nothing for an origin already canonical. Canonicalizing compares keys where they lie.
- The client waits for `CreateGreen { base: Some(_) }` while its daemon lives (`defers_reply`), as for a pin.

**Green.** CI diag run 42371546045, 3 of 3 on each runner:
- ubuntu-latest: 149 to 166 ms, 259 to 306 probes inside, the longest 13.4 to 13.6 ms;
- ubuntu-24.04-arm: 1.71 to 1.76 s, 1,193 to 1,456 probes inside, the longest 7.6 to 10.0 ms.

**Memory.** See BENCHMARKS, "CreateGreen sliced: peak RSS over fs/ and the whole kernel".

**Not changed here.**
- The engine still holds every origin file twice for the green's life, as the head content and as version 0's history
  value. loadtest-scale takes the sharing of those two after W2 (`crate::history::Value`).
- Recovery still walks a held origin in `init_shard` in one call (`walk_held_origin`, now the `OriginWalk` run to its
  end); GAPS' "A daemon's restart can exceed the anchor's fixed recovery budget" (liveness, with loadtest-scale's
  recovery item) covers it.
