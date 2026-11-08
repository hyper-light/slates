# A landing's plan held its shard

**Found** 2026-11-07 by the `landing_fairness.rs` flake under load. On a Mac a presentation held its shard for one
poll of 1,991 ms, and a granted landing's `begin_granted` for one of 489 ms (temporary timing).

**Red** (`crates/server/tests/presentation_fairness.rs`, `presenting_a_large_landing_never_holds_its_shard`: 1,000
files of 16 KiB, an ungranted `land`, a second client probing the shard). On main `e86fb417` the presentation was one
poll of 22.5 to 28.5 ms on ubuntu-latest, 292 to 295 ms on ubuntu-24.04-arm and 6.77 s on a Mac's debug build, with 0
probes answered inside (CI diag runs 42390025344 and 42404201374, 3 of 3 each). On the Mac the 6.77 s were 6,199 ms of
plan and 553 ms of preliminary verdicts.

**Root cause.** `begin_landing` planned and checked a landing in one call:
- the diverged walk of the whole tree;
- every diverged file read and its identity hashed (`plan`, `entry_for`);
- the manifest built;
- for a presentation, every entry checked against the target (`preliminary_verdicts`).

A presentation ran it in its verb (`run_landing`), and a granted landing in its task's first poll (`begin_granted`).
Its writes were sliced in AUD-29-25; its beginning never was.

**Fix.**
- `slates_vfs::base::DivergedWalk`, `slates_land::manifest::PlanWalk` and `slates_land::engine::Beginning` step the
  walk, the plan and the verdicts a unit at a time. `begin_landing` is the same run to its end.
- A presentation defers its reply to a task stepped through `image::sliced`, on an implicit snapshot of the head.
- A granted landing's plan runs in slices inside its task, the lease kept alive between them (`Planning`,
  `KeptLease`).

**Green.**
- Presentation: CI diag run 42404201459, 3 of 3. ubuntu-latest took 155 to 286 ms with 174 to 303 probes inside;
  ubuntu-24.04-arm 660 to 670 ms with 481 to 493 inside; the longest probe 24 ms.
- Granted landing: its test passes on CI and the Mac. On CI's quiet runners an unsliced plan of these files is shorter
  than half the sliced writes, so the test cannot go red there; its red is the Mac's measurement above.
