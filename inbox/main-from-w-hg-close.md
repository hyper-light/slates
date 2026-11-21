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

## C1 update (~20:50): hg's push on the real store, built on branch agents/w-hg-c1; its by-use test blocked by the store
- **3ffb95c4b:** with `--vcs-store`, the push's driver lands the changeset first (durable; Record = v6 head, Payload =
  the changegroup moved in, never copied), then commits the register entry holding a v7 reference (payload identity
  and length, sequence). A recovery or rebuild meeting a v7 record reads the payload back by `StoreHandle::piece` and
  replays it through the sectioned intake, a unit a slice (`ReplayRefused::Unlanded` where it cannot).
- **a9620fbb1:** the Texts section lands with it (w-perf's `slates_store::texts::encode`, fa5fd0023).
- **Red-first by use:** `stock_hg_pushes_land_on_the_changeset_store_and_a_restarted_daemon_replays_them` (stock hg
  7.2: push, `hg_push.landed`, kill -9, clone after the restart). It's **blocked by the store itself on this Mac**:
  "the store's engine: invalid argument: a memtable past 4 GiB", so `repository create` answers Overloaded before any
  push. Reported to w-store (inbox/w-store-from-w-hg-memtable-2026-11-20.md). It lands on main once that's fixed and
  the test passes.

## C1 hg onto the changeset store: the push LANDED on origin/main 242f4a173 (~21:20); the import is the rest
- **Commit:** 242f4a173 (branch agents/w-hg-c1 merged).
  - A push to a daemon with `--vcs-store` lands as one changeset before its entry: Record (v6 head), Payload (the
    changegroup, moved in), Texts (w-perf's encoder).
  - The entry holds a v7 reference.
  - A recovery or rebuild reads the payload back by `StoreHandle::piece` and replays it a unit a slice.
- **Test, by use, red first:** `slates-cli --test hg_interop`
  `stock_hg_pushes_land_on_the_changeset_store_and_a_restarted_daemon_replays_them`: stock hg 7.2 pushes two
  changesets, `hg_push.landed` counted, the daemon is killed with SIGKILL, and a clone after the restart holds both.
  With w-store's bc0677af9/71db6d157 the store accepts landings on this Mac.
- **Suites (local macOS):** slates-hg lib 377/377; slates-server Mercurial tests 55/55; hg_interop 14/14 against stock
  hg 7.2.
- **Not yet: `repository import mercurial` onto the store.** The import still keeps whole texts, which is R2's
  5.73 GB, w-perf's to measure. Taking it next as the rest of C1: one changeset an import through the sliced landing,
  the revlogs' deltas as Payload, Texts per version, no whole texts kept. Then C1 closes whole.

## C1 step 0, the no-store commit: LANDED on origin/main 8cb10f986 (~00:20, 2026-11-21; base origin/main 40c3f8d1e)
- **Refusal:** a daemon without `--vcs-store` refuses `repository create --tool hg`, typed `Unsupported` naming
  `--vcs-store`. It's one `repository::no_store_for` for Mercurial and Subversion alike (w-svn's svn refusal folded
  in). Red test: `repository::tests::a_daemon_without_a_store_refuses_mercurial_and_subversion_naming_the_flag`
  (hg and svn refused naming the flag, nothing recorded; git created).
- **v2 path removed:** `unbundle_in`'s register-held history and `record_sliced`. A push's entry is the v7 store
  reference alone, and a landing without one is refused `UNRECORDED`.
- **One shared helper**, `crates/server/src/test_store.rs`. It is w-svn's file (`TestStore::start`,
  `audit_daemon_stored`, routed `created`/`create_on`). Mine merged into it, adding only what a test's synchronous
  shard history needs:
  - `audit_on_stored_shard`.
  - `land_blocking`: waits for the store's owner on a thread of its own, because a shard's own thread can't wait on a
    wake its runtime delivers.
  - `create`: a creation in the one landing order, landed then recorded in place. It's `repository::create_blocking`
    over `creation_changeset` and `recorded_after_landing`, the two halves `land_then_record` now runs; production
    order is unchanged.
  - `TestStore`'s drop doesn't join while a test unwinds, so a failure shows its message instead of hanging.
- **Tests moved onto the store:** every hg test moved onto store-backed daemons. Replays of stored pushes run through
  the sliced rebuild on a running daemon, since the sync `rebuild_whole` can't read the store. hg_interop's anchors
  each hold a scratch store.
- **Tests, local macOS:**
  - slates-server `repository::`, `vcs_store::`, `vcs_ssh::`, `vcs_http::hg`, `identities`, `git_hooks`, `git_push`,
    `t_a729`: 280 passed, 11 failed. The failures are below.
  - After the final merge: the hg subset 92/92.
  - slates-cli `hg_interop`: 14/14 against stock hg 7.2.
  - cli `a_repository_the_cli_creates_is_found_by_name_on_the_vcs_http_edge`: passed.
- **Siblings, not mine:**
  - These fail alike on a clean origin/main 68ce3d585: run alone, `--test-threads 2`, in 3.8 s, deterministic. A
    submit is refused `Overloaded` by `vcs_store/paths/whole_build.rs`'s park bound
    (`repository.rs:1577` "not accepted", `vcs_store/paths/tests.rs:127`):
    - `repository::tests::{a_destroyed_branch_is_released_a_node_a_unit_and_its_source_is_pinned_until_then,
      a_recovery_rebuilds_every_member_the_way_it_joined_and_fences_what_its_source_cannot_give,
      branches_share_their_lineage_and_each_wrong_verb_is_refused_by_its_kind,
      each_verb_moves_the_names_log_and_each_members_version_its_own_log}`
    - `vcs_store::commits::tests::{t_a664_live_a_submit_waits_parked_for_its_greens_whole_build,
      t_a664_p3_made_commits_name_the_one_before_and_are_made_again_from_the_log}`
    - `vcs_store::paths::tests::{t_a664_holders_a_successors_rebuilt_repository_holds_each_greens_map,
      t_a664_p3_a_branchs_first_version_derives_from_its_sources_map, t_a664_p3_a_store_made_again_holds_each_greens_map_again,
      t_a664_p3_every_version_of_a_git_green_keeps_its_map_of_gits_files}`
  - `git_push::tests::t_a752_a_push_over_a_flat_head_builds_its_work_a_unit_at_a_time` is load-bound on this MacBook:
    run alone at load 27 it passed once and failed once ("the update's work took 714 quanta for 1500 files").
  - Fixed in passing: `repository::tests::a_subversion_repositorys_policy_holds_its_six_new_points_and_its_hooks_env`.
    It created svn on a store-less daemon, which C2's refusal rejects, so it was red on main since w-svn's 0703e62d2.
    It's now on a stored shard.
- **Next, C1 step 1:** file texts read through the store (`StoreHandle::text`/`text_at`), the served view first, async
  lends with no sync path.

## C1: a changeset replayed from its payload a piece at a time: LANDED on origin/main d571ffbca (~00:55, 2026-11-21)
- **What it does:** a replay (recovery or rebuild) holds no payload whole. It reads the store's pieces into a forward
  window as the replay wants them.
  - `slates_hg::framing` is the one walk of a changegroup's chunks, over a `Source` that answers a range or `Wanted`.
    A whole decoding counts, then builds, charged exactly as before.
  - The intake reads deltas through the walk (`Stepped::Wanted`).
  - `record::Replay::begin_sectioned` / `step_sectioned` frame and take a changeset.
  - Server: `vcs_store::hg_piece_step` and `fill_window` (`StoreHandle::piece`); `payload_of` is gone.
- **Red first:** `repository::hg_push::tests::a_landed_push_replays_from_its_payload_a_piece_at_a_time`. A
  789,956-byte payload was read in 14 fills, the window at most 193,859 B (one 128 KiB delta, a 64 KiB piece and a
  header). Before, the payload was read back whole. Counters: `vcs_store.hg_replay_wants`,
  `vcs_store.hg_replay_window_most`.
- **Also landed:** the five `vcs_http::daemon_edge` Mercurial tests red since 8cb10f986, through w-git's C5 part 2a
  (every daemon there is stored); GAPS' entry closed.
- **Tests, local macOS:**
  - slates-hg lib: 382/382.
  - slates-server `vcs_http::`, `vcs_ssh::`, `repository::`, the vcs_store hg/store/paths/upload sets, identities,
    `lfs::`: 264 passed, 2 failed. Both are git tests of main's daemon_edge on stored daemons whose answers pass their
    12 s client read at load 22 (`a_partial_clones_lazy_fetch_...`, `a_push_of_a_long_history_...`): w-git's C5, not
    hg.
  - slates-cli `hg_interop`: 14/14 against stock hg 7.2.
- **Still open from me:**
  - The order question (inbox/main-from-w-hg-c1-order.md): 3, then 1, then 2.
  - The import-as-changesets shape (inbox/main-from-w-hg-c1-import-design.md), for main and w-store.
  - Answered w-perf's stream-clone questions (inbox/w-perf-from-w-hg-serve.md).

## C1 step 3, first part: an import written as a push's changegroup: LANDED on origin/main b73c0a570 (~01:30)
- **What:** `slates_hg::import::emit` (`Import::emitting`, `take_segment`). As the import walks, each revision is
  written as its revlog stores it: the chunk against its delta parent (7.2's `deltaparent`), nothing diffed or rebuilt.
  The stream is cut into segments (only between chunks), each naming its texts as a push's Texts section does.
- **Test:** for each of the five stores 7.2 wrote, the segments concatenated and taken through the push's intake give
  the import's index row for row (`import_tests::an_import_writes_the_changegroup_a_push_of_its_history_brings`).
  slates-hg 383/383.
- **Waiting on your ruling** (inbox/main-from-w-hg-c1-import-design.md, follow-up): each segment's Record, (i) replay
  through the intake or (ii) index rows by identity (recommended: restart stays as cheap as today). Then the server
  lands the segments as a run and the import's entry names it.
