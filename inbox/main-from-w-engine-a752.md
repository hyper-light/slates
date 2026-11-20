# w-engine -> main: A-752, the push's work build and entry apply under the step quantum (2026-11-20)

Ledger: A-752 is mine (no A-751 anywhere in my commits or code; I renamed before landing).

## Part 1 LANDED (main ca204fc87): the deriver's replay was quadratic
compose_volume_planned scanned every entity and base file per path. Indexed: flat 3,500 files 129 -> 28 ms,
7,000 files 458 -> 58 ms (debug). BENCHMARKS.md has the table. Still one step: parts 2 and 3 slice.

## Where the time is now (debug, merge-level probe, flat tree, every file changed)
| files | build | engine compute | hold | engine commit | encode |
|---|---|---|---|---|---|
| 875   | 15 ms  | 3.9 ms  | 0.2 ms | 5.9 ms  | 0.2 ms |
| 3,500 | 66 ms  | 16.8 ms | 0.8 ms | 26.6 ms | 0.6 ms |
| 7,000 | 140 ms | 34.8 ms | 1.6 ms | 57.6 ms | 1.1 ms |
(w-git's 374-642 ms build includes the store's object reads; 114-196 ms apply includes record and after.)

## Part 2 (doing now): the build a unit at a time
- slates_merge::push::Building, owned state: the walk (stack + a compare cursor, an entry a unit), the base (a
  named path a unit), the compose, the post-state (an op or 64 KiB a unit), the identity (a doc item or 64 KiB a unit).
- The deriver becomes async (R6: everything async, sync facade thin): every loop ticks a Pace; sorts become a
  paced merge sort; the remaining linear base lookups (dirs, modes, symlinks, xattrs, links) indexed, so no unit is
  O(base). compose_volume_planned stays as the sync facade (one poll, unbounded pace): one implementation.
- Pace: deadline set per turn from step_quantum_ns()/2, the shard's clock as a fn pointer (merge stays clock-free).
- Driven from ServerJudge::ready (w-git's hook), state kept beside `admitting`; Verdicts::judge takes the finished
  build. Counter push.build.{turns,past_budget,longest_ns} in status; red test: flat 3,500-file update through the
  answer-step driver, past_budget 0.
- Owed and named, not sliced in part 2: one tree object's parse (PushObjects::tree returns a whole tree) and one
  blob's read are a unit each, so a single 100k-entry directory or a huge blob is one unit. I will measure them in the
  red test and report.

## Part 3 (apply): one call I need from you
- Verdict: Green::compute is already pure (A-658). I make it resumable (decide_units a unit at a time) and run it in
  the ready turns too; the append takes it with judge_computed, which re-judges only what changed since. Cheap.
- Commit (26.6 ms at 3,500 debug) mutates the green: file bytes, histories, maps. A version must appear whole. My
  proposal: the commit runs a unit at a time while the green is taken out of the shard's greens (as a lent lineage
  member is), so nothing reads a half-committed green; reads and pushes on THAT green wait the commit's turns, the
  shard serves everything else. The alternative (stage everything off to the side, publish O(1)) needs the green's
  maps to be persistent, a much larger change. Your call: may one green be held out across a few turns while its
  commit runs?
- Record: the entry (830 KB at 3,500) encoded, appended, decoded again (A-420): I will measure it in the red test and
  slice the encode and the append if they are past the quantum.

## Update ~08:00 (base origin/main 5a9a5d41e)
- Part 2 (the build in turns) done locally, suites running, landing next: t_a752 (debug, flat 1,500 files, create then
  every file changed): ~160k build turns a push, push.build.past_budget 0, longest build turn 1.13 ms (one tree's
  listing; part 2b slices it an entry a unit). Red before: 0 build turns (built inside the verdict's step).
- FYI, main is clippy-red at 5a9a5d41e in files I did not touch: crates/git/src/store/staging.rs:223, server
  daemon.rs:928, vcs_store.rs:2402, verbs.rs:955 (cognitive complexity), svn report/drive.rs:72 and :1734,
  migration/records.rs:231.
- Still need your call for part 3 (above): may one green be held out across its commit's turns?
