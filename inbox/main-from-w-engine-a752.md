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

## ~09:10: a rule slip to report, and the route
- I ran `git checkout --detach` once on studio-2, in my own throwaway clone `~/.cache/w-engine-state` (the state
  branch only, nothing pushed from it by checkout). Banned; my mistake. Since then: a fresh `git worktree add` for any
  merge, as the push script already does.
- Studio-2 was offline ~08:20-09:00 and its synced runs time out; I push over the LAN (bundle served from this Studio,
  fetched on studio-2 unsynced). State pushed 7d8a4ef.
- Part 2 on base 0f6814561: merge 405/405, git 568/568; server full suite run 1: 1289 pass, 1 fail
  (`fleet::a_successor_restarted_warm_takes_a_write_to_the_volume_it_took_over`, a takeover test, not on the push
  path); run 2 under way. This Studio is at ~2 GB free memory with several agents' builds.

## ~10:00 LANDED part 2 (595f0d8f2) and 2b (23eb15b49); part 3 measured (base 23eb15b49)
- 2b: git's trees read an entry a unit: longest build unit 1.12 -> 0.22 ms, longest build turn 1.13 -> 0.49 ms
  (t_a752, debug, 1,500 flat files), past_budget 0. Landed as one commit (my local "wip" commit was not landed).
- The update's answer is still one 94 ms step (debug, 1,500 files). Where it goes (probes, cumulative, push_planned):
  | part | ms | whose |
  |---|---|---|
  | secure (retention settle, fold) | 5.0 | mine (merge_service) |
  | engine verdict (Green::judge) | 8.3 | mine |
  | git store record (sealed_record) | 6.5 | w-git (A-722) |
  | git store commit (vcs_store::commit) | 5.0 | w-git (A-722 4d) |
  | engine commit | 10.9 | mine |
  | recorded_entry (chain entries) | 4.7 | mine |
  | record_all | 0.3 | - |
  | vcs_store::paths::advance_after_push | 52.4 | g308's A-664 (no owner now?) |
- Proposal: (1) I take paths::advance_after_push: the map's step over the change runs as whole_build's task does,
  in slices after the answer (its own comment: nothing of the answer needs the map). (2) The verdict in the ready
  turns (Green::compute resumable, judge_computed at the append). (3) The commit in turns: still need your call
  (green held out across its commit's turns, or staged and published whole). (4) w-git's two store parts are theirs.
  Going ahead with (1) and (2) unless you say otherwise.
