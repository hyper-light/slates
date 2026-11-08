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

**Since: the granted test's stall in Stack B1's gate was the operating system's, not the shard's.** The test
`a_granted_landing_never_holds_its_shard_while_it_is_planned` failed once in B1's gate. A probe was `Stalled { after_ns:
1000000000 }`: the client's wall-second reply deadline (clients, A-232). That was on a Mac at load 150 to 217. Both tests
now name each probe that waits half that deadline or is given up. They name it by the shard's own record over the wait:
its CPU time (`Daemon::shard_cpu_ns`), its steps and longest step (`shard_pulses`), and its run state sampled every
millisecond (`slates_rt::registry::shard_run_state`, macOS's `thread_info` run state). A CPU clock alone cannot tell a
preempted step from one blocked in a call. Both stop the clock. On main `4811191a` beside 144 `yes` spinners on 18 CPUs
(load 234 to 369), all 13 runs passed:
- 62 probes of 504 to 982 ms were named, every one starved. Of the shard's 6,582 samples, 6,581 were runnable and 1 was
  blocked in a step; 0 were parked.
- The shard ran a median of 2 ms of its own CPU per wait, at most 164 ms, at most 33 % of the wall.
- The closest call was 982 ms, one step of 973 ms with 39 ms of CPU, runnable in 176 of 176 samples. That was the B1
  failure's shape, 18 ms short of the deadline.

An earlier cut of the naming read such a step as "blocked in a call" by its CPU alone, and the run state showed it
preempted.

So the stall was (b): a live, starved shard behind the client's fixed wall-second deadline, which is the clients' (A-232).
Beside it: 17 of the 62 waits held 100 to 164 ms of the shard's own CPU in at most 8 steps. The shard's wake estimate (4
to 57 µs) does not explain that. Two suspects read from a profile were wrong. A mutating verb's checkpoint costs about
1.4 to 1.9 ms at these 1,000 files in debug, about 100 times too small (GAPS, "A checkpoint costs the whole shard"). The
snapshot's log walk is a real defect, fixed separately, but 30 of 124 waits still held 100 ms or more without it
(`2026-11-08-a-snapshot-decoded-the-log-it-released.md`). A per-poll CPU timer then named the poll:
`return_dead_recovery`, reached from `reap_loop`'s tick through `seal_idle_content` and `purge_if_idle`, 115 to 171 ms of
CPU in one poll. Perf's slicing of that give-back (`a30e4209` on `agents/perf-b3`) took it to 0 of 411 named waits at 100
ms or more, the highest 78 ms, on the same test beside the same 144 spinners.

That slicing has a cost this test measured and its owner has: a granted landing is about five times slower under the same
load with `a30e4209` than without (median 20.9 s against 4.3 s, 8 runs each alternated at load 60 to 209), and landings
past the test's 60 s grant term come back `partial`. Why is not yet measured.
