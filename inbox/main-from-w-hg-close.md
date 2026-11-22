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

## C1 step 3: the import as changesets: LANDED on origin/main a693c49e4 (~07:40, 2026-11-21; base e0b677380)
- **Ruling:** segment records hold the index rows by identity (yours of 01:16).
- **The landing:**
  - The import's changegroup is cut into segments of one trunk node of the store's memtable.
  - Each lands as one changeset between turns: Record v8 (its revisions' rows by identity), Payload, Texts.
  - The register's import entry names the run (v9: first and last sequence). Its writing stage is gone.
- **The replay:** recovery (`replay_hg_run`) and rebuild (`rebuild_hg_run`) read each segment's Record back and append
  its rows by identity, with no payload read and no text rebuilt.
- **Refusal:** an import refused or abandoned sets the next sequence back to the last named (decision 2).
- **Streaming:** an imported repository is stored now, so it streams as a pushed one does.
- **Your oracle:** for each of the five stores 7.2 wrote, the rows the segments' records replay equal the rows the
  push's intake derives from the same changegroup, and the import's.
- **Your red:** a warm restart (t_a729) reads 32 rows from 1 segment record (2,259 bytes) in 23 replay turns, with 0
  payload reads, the record bytes within 256 a row.
- **By use:** stock hg 7.2 `clone --stream` of an imported repository streams and verifies clean
  (`stock_hg_clone_stream_streams_a_pushed_repository_and_an_imported_one`, renamed from "...clones an imported one
  plainly").
- **Tests, local macOS:**
  - slates-hg: 388/388.
  - slates-server, the widened set: 393 passed, 1 failed, the load-bound `git_push t_a752`. The 10 `Overloaded` reds
    are gone on 645937436, as you said, and not listed.
  - hg_interop: 15/15.
- **Removed:**
  - `rebuild_whole`, `make_whole` and `paths::remake` (uncalled now).
  - The two hand-polled recovery tests of a register-held import record (that path is gone; t_a729 holds the run's
    recovery).
- **Next:**
  - C1 step 1 (file texts read through the store, the served view first), then step 2. Then R2 to w-perf.
  - After C1, the push's memory (your 05:12 ruling).

## C1: made changesets on the changeset store: LANDED on origin/main ac25475c1 (~10:10, 2026-11-21; base 8d7dec8df)
- **Your ruling (09:00), one hook and one path:** a made changeset is a changegroup the server builds, taken through
  the push's own intake and landed as a push's changeset: a made record (the green's key and version beside the
  push's head), Payload and Texts, named by a made reference in the register. A replay puts the made node back.
  - Recorded submits go through the shared deferred-submit hook. The changeset is made from a preview of the held
    verdict and landed by a task, then the version and the entry are committed in one step. A bare dispatch lands it
    from a task holding the turn.
  - Edge cases: a store refusal gives the accept up; a fork a client's push made stale leaves the version alone,
    counted.
- **Removed, with no fallback:**
  - the v3 made record (`write_made`, `DELTA_VERSION`, its `formed` reader);
  - `hg_made::recorded`;
  - `HgStore::unstored`, and `State::stored_logs` with it (w-perf's flag): every history is on the store and every
    repository streams.
- **Tests, local macOS:**
  - the made changegroup through the intake equals `fork_with`, row for row (stock 7.2's three changesets);
  - a made record's replay puts the made node back;
  - `t_a731`: two made changesets named by reference on the store, and the rebuild puts the made head back;
  - slates-hg 390/390; the hg server subset 89/89 after the final merge;
  - the widened server set 369 passed, 1 failed (a git daemon_edge fetch that passes alone twice at load 16);
  - hg_interop 15/15 against stock 7.2, the CLI's bookmarks submitting through the hook.
- **Next:** C1 step 1a, the clone walk's file section through the store (StoreWants), then 1b (the pull).

## C1 step 1a: a clone's file revisions through the changeset store: LANDED on origin/main 5d2940ff9 (~11:20, 2026-11-21; base ac25475c1)
- **The walk:** the by-links walk (a clone of every head) reads each file revision from the changeset store through
  w-perf's StoreWants. It asks rows for as many revisions ahead as the step's room holds, then their chunks, between
  turns.
- **What it sends:** each revision as the store keeps it, a stored delta against the node its base was sent as earlier
  in the group, or a stored whole. No whole text is read from the content store or made.
- **Refusals, typed:** a revision with no store row, and a whole answer that can't wait between steps.
- **Lenders:** `HgStores::answer_wants`. The daemon's asks the node's store; a test's lender answers from its view.
- **Tests:**
  - the clone-walk test: every revision after a log's first is a delta against the one sent before it, and the texts
    rebuild equal;
  - by use, a plain stock `hg clone` asks the store (0 asks before 1a) and verifies clean;
  - slates-hg 391/391, the hg server sets 200/200, hg_interop 15/15.
- **Open, 1b:** the pull path (`named_changegroup`) still reads whole texts from the content store. It becomes the same
  walk, with your condition (the base on the client or sent earlier), a count of revisions sent whole, and the
  one-changeset-pull-of-a-large-file measurement against stock.

## C1 step 1b: a pull's file revisions through the changeset store: LANDED on origin/main b9fed44e9 (2026-11-21; base 5d2940ff9)
- **The pull:** any fetch the links don't walk still makes its changelog and manifests whole when it begins. Its file
  section is now `serve::getbundle::Pull`, taken a step at a time through StoreWants:
  - the rows of a batch of planned revisions and of their parents;
  - then their stored edits;
  - then any whole texts, one at a time (new `Want::Texts` / `StoreWants::give_texts`; the daemon reads
    `StoreHandle::text` a piece at a time to its end).
- **Your condition:**
  - A stored delta is sent when its base is an earlier revision of its group (sent in this answer), or a parent whose
    changeset the common set holds.
  - Otherwise its whole text, made from its chain, is sent against null and counted: `Walk::whole_sent`, and the
    daemon counter `vcs_store.hg_pull_whole_texts`.
  - A short answer from the store, or a revision with no row, is refused typed.
- **Measured (MacBook arm64, debug, stock 7.2):** a one-changeset pull changing one line of a 40,000-line (~2 MB) file.
  - slates: 554 bytes of changegroup;
  - stock 7.2 pulling the same changeset from its source: 554;
  - none sent whole.
  - Not materially more, so no A/B arm of a delta against p1.
- **Tests:**
  - new: a stored delta sent against a held p1; a sibling-based stored delta sent whole and counted 1; a pull with no
    store row refused by kind; the one-changeset-pull step count is now "a step a unit sent";
  - slates-hg serve 94/94, slates-server hg 69/69;
  - hg_interop: the new `a_pull_of_one_changeset_sends_its_large_files_stored_delta_as_stock_does`, plus both stock
    round trips (HTTP, SSH), pass.
- **For you to run:** clippy on slates-hg and slates-server, and the full hg_interop suite. Red would be a `Want::Texts`
  match left non-exhaustive in a lender I didn't see, or a pull refused "a file revision the changeset store gave
  short".
- **Next (your order):** the other synchronous readers (lookup's `.hgtags`, the push intake's base text), then step 2
  after w-perf's intake change lands.

## Whole place = raw text; lookup's .hgtags and --from through the store: LANDED on origin/main 8759f75b3 (2026-11-21; commit 2e3f99484, base 20c2bc961)
- **Whole place (main's approval):**
  - Both writers (push intake, import emitter) now record a whole's place as its raw text, past the trivial hunk.
    Before, `StoreHandle::text` refused `BadRow` for every hg text.
  - The stream sends a whole as it lies (`text_after_trivial_hunk` removed). Clone and pull write a whole with
    `put_revision_whole`. A non-trivial delta against null is refused typed (`Unverified::NotWhole`).
  - Old stores refuse `BadRow` until re-imported or re-pushed; no shim.
- **Lookup, ruling (A):**
  - `TagsRead` reads all heads' `.hgtags` rows in one want, then texts in one, under `Want::Texts`' `most`.
  - `Producing::Lookup` over SSH and HTTP (0.1, uncompressed). `Batch` reads once via `BatchStep::Store`.
  - The `--from` door (`vcs_store::hg_tags`) parks `CreateBranch` while a task reads.
  - `Repository::tags_text` removed. Counted `vcs_store.hg_lookup_store_asks`.
  - New `hidden-tag` fixture made by stock 7.2.
- **Tests (on the merged tree):**
  - slates-hg 401/401; server hg 71/71, including the red store test (red before the fix), the five-format two-writer
    oracle, and `--from v1` through the door;
  - daemon_edge 28/29, the one failure git's A-713 long-history push, which passes alone.
- **For you to run:**
  - hg_interop: the stock `pull -r <tag>` over SSH and HTTP, a batched `pull -r a -r b`, and the `--from` verb by use
    (not yet written as interop tests; I can add them next if you want);
  - the full hg_interop suite;
  - clippy on slates-hg and slates-server.
  - Red would be a tag lookup refused "a .hgtags text not read", or a stream or clone of a pushed repository failing
    verify.
- **Next:** the intake's base text, waiting on w-perf's answer about intake.rs. Then step 2.

## The push intake's file delta bases through the store, main's (b2): LANDED on origin/main 8f7d388d3 (2026-11-21)
- **What it does:** a file delta whose base isn't the text kept last no longer reads the content store's whole text.
  It is found in this order:
  - the push's own cache (bounded by its room, let go per file group);
  - otherwise rebuilt from the push's own deltas;
  - otherwise read from the store between steps for a revision landed before the push (`Stepped::Store`).
- **Drivers:** the push's sliced intake, and the recovery's and rebuild's piece replays (`answer_wants_over`, over the
  disk alone).
- **Counted:** `vcs.hg.push_base_hits` / `_rebuilds` / `_store_reads` and `vcs_store.hg_intake_base_asks`.
- **Tests:** slates-hg 402/402 and server hg 71/71 on the merged tree. The new red is
  `a_file_delta_base_before_the_last_text_comes_from_the_cache_the_push_or_the_store`.
- **The b1/b2 A/B** is deferred under goal 93's order (performance last).
- **Note:** the final push included a second merge of main, touching `hg_push.rs`. Its re-test is running now; if it
  goes red I fix it at once.
- **Next:** `push/tree.rs`, which needs your ruling (inbox/main-from-w-hg-tree-through-store.md). Meanwhile
  `push/made.rs`, `push/verify.rs`, `Indexed::file`.

## Three more readers of whole file texts gone: LANDED on origin/main a6f4465ba (commit fe30843fc; 2026-11-21)
- **`push/made.rs`:** whether a file keeps its parent's node is told by the texts' names (block and bytes identities,
  `Held::file_names`); `IndexHeld` reads them from the row with no text read.
- **`push/verify.rs`:** the presence check reads rows. The whole `verify` has no production caller and stays the
  intake's test oracle.
- **`getbundle::Store::file` and `Indexed::file` removed:** `file_entry` (the row) is the served view's one file read.
- **Tests:** slates-hg 402/402 and server hg 71/71 on the merged tree. The last merge brought only git's `upload.rs`.
- **Left before step 2:** `push/tree.rs`, after the push spin (your order).
- **Now:** the push spin. Piece A, the intake a window a step, is written with its red test; it compiles in the retest
  build and its own run is next.

## The hg concurrency admission red: FIXED, landed on origin/main 687cd98b1 (commit 521fe3689; 2026-11-21)
- **Cause:** `vcs_store::index_upkeep::slice` lent each merge step the shard's whole admittable room, and the lend's
  `make_room` claimed the whole shared arena pool for one shard (28.6 GB). The other shard was left at capacity 0 and
  admittable 0, and refused even capabilities.
- **Fix:** a merge step is lent `Merge::cut_bytes`.
- **Tests:**
  - the index_upkeep merge tests, red before ("grown 11453071360 of 11453071360") and green after;
  - by use, concurrency at N=2 and 10 passes (debug).
- **Git siblings in GAPS:** restage, seal and `git_hooks`' seal lend whole rooms. w-multirepo told.
- **Next:** the SSH truncation red, then the push spin, then tree.rs.
