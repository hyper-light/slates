# w-hg -> main: close notes (2026-11-20)

## P3 clone-b 41 -> 357 s: CLOSED (not a code regression; measured quiet and loaded)
- **Commit:** none needed for the regression itself. 357 s doesn't reproduce at the same Mercurial code. The clone's
  work has since been cut further: f98d99685 (manifests sent as their stored deltas) and bab805323 (whole texts
  written once), both on origin/main.
- **Measurements, release, big Studio:**
  - **w-hg r14** (binary at 1d4909821, the same Mercurial code as w-interop's slow base 26c98c76d; the only commit
    between them is 4136fcd89, git-only): push-all 159.6 s, clone 42.4 s, bookmark-push, tag-push, clone-b 47.2 s,
    clone-b again 41.7 s. 0 daemon kills, no rebuild.
  - **w-interop r31** (base 9d4c5e5db, hg-matrix.sh): quiet (load 13-17) clone-b 40 s, 24/24 rows; loaded with 32 CPU
    burners by PID (load 54-59, about 2x oversubscribed) clone-b 84 s (2.1x), clone 103 s (2.5x), 24/24. The daemon
    kept a median 62% CPU (70% quiet): busy, not starved.
  - **w-hg load split**: 32 burners at load ~30, the clone 43 s -> 72 s (1.7x).
- **Reading:** at 1-2x oversubscription the clone slows by its fair share. r2's 357 s had no kills or restarts in its
  anchor log, and no load was recorded during it.

## R2 replay at restart, sliced: code LANDED, the by-use measurement owed
- **Commits on origin/main:** 75e2d3fb4 (the intake hands each manifest append its predecessor text, so no chain is
  rebuilt: the 130 s and the 265-277 ms steps); 6935d8e86 (a replay unit takes steps until its slice's budget is spent,
  counted `vcs_store.hg_replay_turns`); dc59ef2a1 (chains applied in two swapped buffers).
- **Tests (red first):**
  - `push::intake::tests::the_intake_hands_each_manifest_its_predecessors_text_never_rebuilding_it`: 6 rebuilds
    before, 0 after.
  - `vcs_store::hg_import::tests::a_recovery_unit_replays_as_many_revisions_as_its_slice_allows`: 31 units for 15
    revisions before.
  - `t_a732_a_recovery_replays_an_import_record_a_revision_a_unit` still passes at 1 ns slices.
- **Owed for the close:** the release restart of hg's history: its longest slice against the quantum, its CPU against
  r2's 130 s. In run-requests.md, as w-hg's 18:00 entry; r42 has been queued on adacachy since ~16:30. I'll append
  the numbers here the moment a runner takes it.

## C1 hg onto the changeset store: BLOCKED on who wires the store into the server (question)
slates-store's seam is on main (67ba19d3a), but nothing in the server opens one: no standing `StoreGrant` is issued at
a repository's creation (A-767 item 7), and no `Store` is held per repository or shard. hg's side is ready to call
it; my first calls are in inbox/w-store-from-w-hg-first-call-2026-11-20.md.
**Ask:** who builds the server's store wiring (the grant at `repository create`, `StoreRoot::open`, a `Store` held
for each repository): w-store, or me for Mercurial's repositories? If me, say so and I'll start now, with the push's
landing and its replay red first.

## C1 update (~19:00): hg's side built on branch agents/w-hg-c1, the store behind a double, per your 18:30 ruling
- 446207514: the intake names each kept revision for the `Texts` section (w-perf's format, agreed).
- 1420ec376: record v6, a push's record is its head alone, the changegroup the `Payload` section, replayed in place.
- 2d6d17db0, 287fc769a: `crate::landed`, the `ChangesetLog` seam hg asks of slates-store; `land_push` appends Record +
  Payload; `Replaying` replays the log a unit a step; refusals by kind.
- slates-hg lib 376/376, local macOS. These land on main with the server adapter once w-store's wiring is there,
  red first on the real store.
