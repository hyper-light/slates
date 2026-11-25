# w-hg -- UI goal 93 part "The Mercurial server built"; implementer; supervisor "slates-7a" (SendMessage to: "slates-7a")
# Policy read 2026-11-19: relaunch-2026-11-19.md, briefs/common.md, briefs/w-hg.md, restart-preamble-2026-11-18.md,
# slates CLAUDE.md §8 (origin/main b63f8a05d). Studio d1323898 via `kiln run --on d1323898... --no-sync -i -- 'bash -s'`.
# Tools: ~/.local/state/slates-fanout/w-hg-tools/ (run-2b.sh, messages). Studio: ~/.cache/g303-target (reused),
# ~/.cache/w-hg-logs, ~/.cache/w-hg-clone (deleted by script); delete g303-* and w-hg-* there at END.

## Piece 1: A-680 part 2b (hg push over HTTP) -- landing
- Worktree ~/slates-work/g303-a680-p2b-land2, branch agents/g303-a680-p2b-land2. Found mid-merge of origin/main
  b63f8a05d (daemon_edge.rs + design resolved, unstaged). Committed merge dfb40c810, pushed. Diff vs main: 17 files,
  the piece's alone.
- Run (2026-11-19, this agent): run-2b.sh over dfb40c810: hg --lib push:: serve::, server --lib vcs_http::
  vcs_store::hg_view repository::hg_push vcs_ssh::hg (4 threads), fmt, cli --test cli stock hg HTTP round trip
  (--include-ignored) + refusal. Output: w-hg-tools/run-2b.out.
- Relaunched w-hg 2026-11-19: r2 (Studio, over dfb40c810) hg --lib push:: serve:: 95/95, server --lib focused 78/78,
  fmt clean, cli stock-hg HTTP round trip + refusal --include-ignored 2/2 (ran, no skip). Un-ignored it (7c71c8048),
  merged main (only git_interop.rs + GAPS: no rerun). LANDED main 354b02d2a. amendments.tsv A-680 row updated.
- Owed: 2c (record 2b's pushes with A-687's kind) after A-687 lands.

## Piece 2: A-687 (durable import + the one Mercurial record kind) -- next
- Merged b63f8a05d then 354b02d2a into agents/g300-a687-import-durable (cdc0b2987; push.rs Stored = Git+Imported+Lfs,
  ReplayRefused::Owed dropped). Squash-landed via ~/slates-work/w-hg-a687-land (agents/w-hg-a687-land): piece
  cd5217d1f, LANDED main 6a876f6ae. Studio r4/r5: hg --lib import 28/28, server focused 84/84, fmt clean.
## Piece 2c: A-680 part 2c (record hg pushes with SHGR Source::Push; replay) -- next
- 2c LANDED main 243b7bfae (piece 23e2789e7; worktree ~/slates-work/w-hg-a680-2c). Studio: hg --lib import 28/28,
  server --lib repository::hg_push vcs_store:: repository::push repository::import 71/71 (new
  a_landed_push_is_replayed_into_the_store_from_its_entry). Remote branch agents/w-hg-a680-2c holds the pre-fmt
  a1a2632dd (amended locally after pushing; not force-pushed).
## Piece 3: A-696 hg push over SSH (g304's branch) -- next
- A-696 LANDED main 3691deb5c (one commit on origin/main; worktree ~/slates-work/w-hg-a696-land). g304's commits
  cherry-picked, tests fixed for main (ToolServices 4 args, borrow fixes). Studio: hg serve:: 63/63, server vcs_ssh
  vcs_http::hg hg_push 81/81, stock-hg SSH round trip 1/1 -> un-ignored. g304's worktree left as found (mid-merge).
## Piece 4: hg `lookup` (briefs/g258.md) -- next
- A-705 hg `lookup` LANDED main 3bcaccf91 (piece e783c3440; worktree ~/slates-work/w-hg-lookup). Local (this Mac,
  ~/.cache/w-hg-local-target): slates-hg serve:: 66/66 (3 new lookup tests), cli stock_hg round trips 2/2 with
  `identify -r tip`. Stock 7.2 answers recorded on the Studio. Owed: tags beyond tip.
## Piece 5: stream clones parts 1-3 (briefs/g083.md; progress/g083-w1-under-1ms.md) -- next
- A-706 stream clones part 1 (slates_hg::render) LANDED main 3fee2f575 (worktree ~/slates-work/g083-hg-stream-p1,
  branch agents/g083-hg-stream-p1). Local: slates-hg --lib import 29/29 (new round-trip test); stock 7.2 hg verify
  passed on rendered dot + states (one-off, uncommitted writer). Owed in part 1: deltas/compression, render as
  versions commit (sliced).
## NEXT: stream clones part 2 (`stream_out` over render) then part 3 (stream2/streamreqs, stock `hg clone --stream`
  over the daemon edge + hg verify). Worktrees held: ~/slates-work/w-hg-lookup (landed; removable),
  ~/slates-work/g083-hg-stream-p1. Local target ~/.cache/w-hg-local-target. Studio: ~/.cache/g303-target,
  ~/.cache/w-hg-clone, ~/.cache/w-hg-logs, ~/.cache/w-hg-run-2b.sh to delete at END.
- END (context short) 2026-11-20: Studio dirs (g303-target, w-hg-clone, w-hg-logs, scripts) and the local target
  ~/.cache/w-hg-local-target deleted. Worktree ~/slates-work/g083-hg-stream-p1 kept (landed, clean) for part 2.
  Part 2 plan: serve `stream_out` (7.2 generatev1: `0\n`, `<files> <bytes>\n`, per file `<encodedir(name)>\0<size>\n`
  + bytes) over render's files through the server's lender (HgStores needs index+content access for the render);
  advertise `stream`/`streamreqs=generaldelta,revlogv1,sparserevlog`; then part 3 stream2 + stock `hg clone --stream`.
- 2026-11-20: A-707 stream part 2 (`stream_out`, streamreqs) committed on agents/g083-hg-stream-p1 (pushed, NOT landed):
  local hg serve:: import 96/96. Before landing: stock `hg clone --stream` over the daemon edge + hg verify; docs
  lines; A-707 row (not yet claimed in amendments.tsv).
- PAUSED for main's TOP PRIORITY: hg push of Mercurial's own repo (57k changesets, 84 MiB) -> 413 (unbundle body past
  the request's room, vcs_http/hg.rs). Fix as A-704 did for git, sliced; failing tests first; proof w-perf's
  crates/cli/bench/hg_scale.sh (branch agents/w-perf-p2); tell w-perf (slates-work-89) when landed.
- A-711 (hg push room = shard's admittable, HTTP+SSH; past it pusherr) LANDED main bc2803d66 (worktree
  ~/slates-work/w-hg-a711). Local 24/24 incl. new daemon_edge large push test (red 413 first). Told w-perf to rerun
  hg_scale.sh. A-712 claimed for stream part 2 (commit ed54ff9b1 on agents/g083-hg-stream-p1; the A-711 commit there
  too, cherry-picked to main -- re-merge before landing).
- NOW: import room (ImportRefused PastRoom{278528} on Mercurial's repo; hg_import::room_of = one region, reserved as
  retention at begin). Derive per repository, sliced (the import is already turn-sliced).
- 2026-11-20: w-perf rerun of A-711: 413 gone; 57k push trips watchdog (1152 ms). Measured hg repo manifests: 57,007
  revs, 9.4 MB deltas, 5.5 GB full texts -> slates' full-text storage (index content, push Verified, SHGR records) is
  the blocker. Sent main a 4-piece proposal (delta storage, delta records, sliced intake, per-repo import room);
  awaiting word; starting (4)/(3) meanwhile. One-off import least-room measurement running (bg task byk42vq42).
- A-712 stream part 2 (`stream_out`, streamreqs) LANDED main 7405f9508 (piece f78ff53f0; worktree ~/slates-work/w-hg-a712).
  Local hg serve:: import 96/96. Owed: stock `hg clone --stream` over the edge (part 3 with stream2).
- 503 tests fixed (helper lands the import; heads = stock served heads) LANDED main (piece 3a59b5776); daemon_edge 13/13.
- Main's order: (3) sliced intake + (4) per-repo import room, BUILT with content-once: decode each revision as it
  arrives, rebuild against a stored base, check node, store once; file texts named by the green's ContentIdentity;
  working set bounded independent of push size; admission reserves only the working set, content charged as stored
  (A-714 retention); cite w-git's §4.17.1 rule. Then (1)+(2) manifest/changelog deltas + SHGR v2. Numbers: claim by
  pushing amendments.tsv on agents/fanout-state (next free A-720 at last word). One-off import-room measurement was
  killed at 30 min (release build + 5.5 GB of manifest texts); not rerun.
- A-720 claimed (ledger pushed). Part 1 (slates-hg push::intake, sliced engine) LANDED main (piece 56952183f). Local push:: 41/41.
  Note: ran `git checkout -b agents/w-hg-a720` in ~/slates-work/w-hg-a712 by mistake (rule breach; no files changed).
- NEXT part 2: server drives Intake in slices (with_state per slice, yield between, quantum = step_budget_ns), one
  intake per repository at a time, prepare/classify/tree over IndexHeld(fork), land with the fork, record from fork.
  Storage: point at w-perf's identity table when it lands (crates/merge slice 1); no second store meanwhile.
- A-720 part 2a (server drives the intake in slices, async HgStores::push, own fork per push, generation stamps,
  landing judged at landing, record from fork) LANDED main (piece 6e1896f22, merge 49439c77e). Local: hg push:: import
  71/71, server focused 44/44 (incl. after final merge). Removed ~/slates-work/w-hg-a711; cleared target's release +
  incremental (disk was 17-18 GB free).
- NEXT: part 2b (re-base a stale fork by node at landing instead of MOVED), then part 3 (import room per repository),
  then S5 per main's convergence plan (file texts as green files by identity, changelog/manifest deltas, rows by id).
- A-720 part 2b (stale fork re-based by node at landing; land never swaps in a stale fork; side-by-side pushes land)
  LANDED main (piece 64d457979, merge 5df3c5a18). Local: hg 72/72, server 45/45 (32/32 hg filters on landed tree).
- NEXT: part 3 (import room per repository; ImportRefused PastRoom{278528} on Mercurial's repo), then S5.
- A-720 part 3 (import room = shard's admittable, charge trued up to Import::holding each turn) LANDED main (piece
  08d45661e, merge 99c0f7877). Local: server import/hg 15/15 + 40/40, hg 72/72. By-use proof owed to w-perf.
- SSH test fix (stock push over SSH awaits steps; was "a step that waits") LANDED main (b1ec1fe00, merge 7477a94a7);
  server `-- hg` 54/54 on landed tree. NOTE: use filter `-- hg` (my `vcs_ssh::hg` missed vcs_ssh::tests::hg).
- Target trimmed: 24 GB -> ~1.3 GB (incremental, release, orphaned *.rcgu.o); disk 78 GB free.
- QUEUE NEXT: S5 (main's convergence plan; main will send hg's step details), then the server-made hg changeset
  (hg_made::make hook in verbs::judge_submission, per w-shared's seam message), then the inventory list.
- A-731 (server-made hg changeset: slates-hg push::made, vcs_store::hg_made, Source::Made, HgStore::made, head_of)
  LANDED main (piece 9ed874650, merge 36c65b2a3). Local: hg 74/74, server `-- hg vcs_store:: repository::` 130/130.
- NOW: S5 = A-729 (claimed): content once by identity (main's 6-point spec: file texts by identity homed via homes.rs,
  read via A-727 Cow seam; manifests/changelogs as revlog-shaped deltas, snapshot interval derived; SHGR names
  identities only; intake rebuilds against stored base; replay re-indexes by identity; proof w-perf's import).
- A-732 crash-loop fix (record::Replay + record::Writer; recovery replays hg records a revision a unit; import's
  Stage::Writing) LANDED main (piece 165bcd2a7, merge e7353d339). Bug record docs/bugs/2026-11-20-a-large-mercurial-
  import-crash-looped-the-daemon.md. Local hg 74/74, server 131/131 (+ flaky git test t_a664_live... passes alone).
- NEXT (main's order): (2) the whole 57k push over HTTP still spins shard 0 ~183 s in (w-interop, release 1533d52b,
  RSS 13.9 GB): find the unsliced step (landing/record write/classify), failing test first; then (3) S5 (A-729).
- A-732 part 2 (push record written in slices before landing: record::Writer::begin_kept, hg_push::record_sliced)
  LANDED main (piece d8b23d0e5, merge 5a917afcd). Local hg 75/75, server 136/136 (61/61 on landed tree).
  Still one step: the landing's append of an entry as large as the record -> S5 (A-729) fixes by identity-only records.
- NOW: S5 (A-729).
- A-729 part 1 (manifests kept as deltas in the index; delta::encode_lines; HgIndex::revision_text) LANDED main
  (piece 2a42df233, merge d7713a435). Local hg lib 354/354, server 137/137, cli stock_hg 4/4.
- NEXT: (1) slice vcs_store::rebuild's replay (post-recovery spin per w-interop rerun); (2) S5 part 2 (SHGR v2:
  names + changegroup deltas; replay re-intakes).
- A-732 part 3 (sliced Mercurial rebuild: VcsStores::rebuilding, replay_sliced task; rebuild_whole for git/svn/tests)
  LANDED main (piece 4fa835b3f, merge 0f756ec77). Local server 140/140.
- NOW: S5 part 2 (A-729): SHGR v2 = names + changegroup deltas; replay re-takes it through the intake.
- 2026-11-20 (after compaction), LANDED on main, each with focused tests (target now ~/.cache/w-hg-local-target.noindex,
  CARGO_INCREMENTAL=0):
  - A-729 part 2 push: SHGR v2 = changegroup as arrived, replayed through the intake (b1d001601). Proven by use:
    w-interop 57,035-rev push 142 s, RSS 5,460 MiB vs 13,450, no spin.
  - A-729 part 2 import: SHGR v3 manifests as index deltas (5ad8dea09); made records v3 manifest delta (669bf99e4);
    import file revisions as one-hunk deltas vs previous rev (de49b6bd4; w-perf found 4.66 GB record + one-step
    landing append spins shard 9; C run queued). OPEN (main's ruling asked): bounded import append (entry parts, or
    identity-only records after A-727 a3).
  - A-575 sibling: hg branch verbs name bookmarks (62fb88997).
  - A-745 (claimed): fetch answer in slices. Design rule §4.17.1 + red (3daa8b369); piece 1 Answering/Walk by links
    (d0749fe2d); 2a HTTP streamed (658eb7d9c); 2b SSH (2aeeac954); rule 4 generation hold (216c57ad2); piece 3
    stream_out streamed (84a596819); stream2 findings in GAPS (f14562e4c). Owed: stream2, matrix rerun (w-interop).
  - A-731 copies in made changesets (eeabd1dfe).
- OPEN with main: S5 homing design (homes.rs first consumer + async read path) — asked, awaiting ruling.
- NEXT: inventory by user impact: pushrebase (server-made changesets over a moved head), clone bundles, ...
- 2026-11-20 (later), LANDED: ssh-clone stall (rekey with a full outbox; key time fixed at RFC's 1 h; bytes-driven
  volume rekey proved under ChaCha20, 53a81211d); identity-only import records SHGR v4 (306214c57).
- Item 5 (A-575 `--from`, main's ruling): Mercurial lookup order (bookmark, tag, branch tip) via
  slates_hg::serve::lookup::resolve_name; slates_hg::tags (7.2 fold, 300 vectors from 7.2's own functions);
  served lookup resolves tags. Commit e531594fa, merged with origin/main 2cf1b1d35; focused tests rerunning as kiln r24.
- Item 2 clone memory: r21 big Studio, base 359da2388: push peak 7,687 MiB, after-push 8,109; first HTTP clone ->
  12,143 MiB, held after; ssh and stream clones add nothing. r23 (vmmap + heap per phase, second HTTP clone) running.
- Local target deleted (main, disk low): every cargo run now through kiln, no `on`.
- NEXT: item 2 bound (red first), item 6 (forced new-head push over a moved line), item 4 S5 head texts.
- 2026-11-20 afternoon, LANDED (each red first or held by a stock oracle; focused tests local):
  - 46951a489: lookup and every whole answer get the fetch's room (clone-rev's PastAdmission).
  - 648a46d1a: the changeset A/B example plus its BENCHMARKS section (counts and release timings).
  - 75e2d3fb4: A-732 part 4, the intake hands each manifest append its predecessor's text (no chain rebuilds).
  - dc59ef2a1: revision_text applies a chain into two swapped buffers.
  - 6935d8e86: A-732 part 5, a replay unit takes steps until its slice is spent (vcs_store.hg_replay_turns).
  - 13523dbe3: item 6(a) proven, a forced new head on a moved line held to stock 7.2's push -f.
- OWED: the restart of hg's history measured again, release (r42 queued on adacachy; the Studios are unreachable
  for sync). Item 6(b) (the lock-free rebase) waits on main's ruling. The changeset shape waits on w-store's arm.
- 2026-11-20 evening, LANDED:
  - f98d99685: a clone sends manifests as their stored deltas.
  - bab805323: whole texts written into the answer once.
  - Branch agents/w-hg-file-deltas-held (c9c3ef73b): the per-file arm, for w-store's A/B.
  - Inbox notes answered: w-store's stored form, w-sizing's numbers.
- WAITING: r42 (the restart and clone measurement, queued on adacachy; also in run-requests.md); main's ruling on
  item 6(b); w-store's piece 3, before hg moves onto the changeset store (the C item, with R2's red test: an import's
  committed content against the .hg's bytes).
- 2026-11-20 ~19:00, C1 (main's 18:30 ruling: hg's side against the seam, the store behind a double; land on main
  once w-store's wiring is there). Branch agents/w-hg-c1 (worktree ~/slates-work/w-hg-c1), each red/oracle-tested,
  slates-hg lib 376/376:
  - 446207514: the intake names each kept revision for the Texts section (path, node, text BLAKE3 and length, base
    identity, place in the payload).
  - 1420ec376: record v6, a push's record is its head alone, the changegroup the Payload section, replayed in place
    (Replay::begin_sectioned).
  - 2d6d17db0 + 287fc769a: crate::landed, the ChangesetLog seam, land_push (Record + Payload), Replaying a unit a
    step; refusals by kind (Missing, Record, Unreleased).
  - Next on C1: the server adapter (slates_store::Store as ChangesetLog) and hg_push::land / recovery onto it, once
    w-store's wiring lands; the Texts section once w-perf's encoder lands.
- R2: r43 placed itself on this MacBook (load 116) and was stopped (no measurement here). r42 is still queued on
  adacachy.
- ~20:50 C1 on branch agents/w-hg-c1: 3ffb95c4b (land first, v7 reference in the entry, replay from the store by
  piece), a9620fbb1 (Texts section). By-use test blocked: the store refuses every landing on this Mac (memtable past
  4 GiB; reported to w-store). Lands on main when it passes.
- ~21:20 LANDED 242f4a173: C1's push onto the changeset store (by-use test with stock hg 7.2 passes; suites green).
  NEXT: the import onto the store (rest of C1; R2's peak).
- ~21:45 main's ruling (inbox/w-svn-from-main-c2-rulings.md, decision 1) applies to C1: no repository without
  --vcs-store (a typed refusal naming it), no register-held history path for hg, and tests on store-backed daemons
  through ONE shared helper (repository::store_tests' directory, made shared with a removal guard). Not small for hg:
  every hg server test runs on a store-less shard through the sync push path (unbundle_in). Plan: the very next
  commit after C1 lands. Recovery rule already matches decision 2 (next sequence = named + 1; unnamed changesets
  dropped).
- C1 remaining order:
  1. file texts read through the store (T rows) instead of the content store;
  2. the intake keeps no whole text with a store;
  3. the import as changesets (R2).
  Then the no-store path dropped.
- 2026-11-21 ~00:20 LANDED 8cb10f986: C1 step 0, the no-store commit. One `repository::no_store_for` refuses hg and svn
  without --vcs-store. The v2 path is gone. hg tests are on store-backed daemons through the one shared helper
  (w-svn's test_store.rs plus mine: audit_on_stored_shard, land_blocking, create → repository::create_blocking).
  Close note written, with the siblings: 10 failures already on main 68ce3d585 (whole_build's park bound) and the
  load-bound git_push t_a752.
- ~00:30 asked main (inbox/main-from-w-hg-c1-order.md) to order C1 as 3, then 1, then 2: step 1 alone can't serve an
  imported repo, whose texts aren't on the store until 3.
- ~00:45 step 3 design sent to main and w-store (inbox/main-from-w-hg-c1-import-design.md): the import lands as a run
  of bounded changesets whose payloads, concatenated, are one cg3 changegroup; the register's one import entry names
  the run; replay goes through the push intake reading pieces.
- NOW: the piece-reading intake (needed by both 1 and 3).
  - slates_hg: a `Source` (bytes by range, or Wanted{range}) and a forward `Window`.
  - The changegroup framing decoded in steps from a cursor over a Source.
  - Intake::step reads its delta through a Source; a step asks for its bytes before changing any state.
  - Server replay (vcs_store replay_hg_referenced / replay_sliced, payload_of): a window filled by
    `StoreHandle::piece` on Wanted. No whole payload is held.
  - Red test: a replay's peak bytes against the payload's length.
- 2026-11-21 ~00:55 LANDED d571ffbca: the piece-reading replay (framing over a Source, intake Wanted, Replay piece
  mode, server fill_window; red test 789,956 B payload in 14 fills, window at most 193,859 B), and the daemon_edge hg
  tests green via w-git's C5 part 2a. Close note written. Waiting on main's ruling for C1's order (3, 1, 2) and the
  import's shape; next by default: step 3, the import as a run of changesets.
- 2026-11-21 LANDED 6a0600ff1 (segment records v8, run reference v9, main's oracle on all five 7.2 formats) and
  a693c49e4 (C1 step 3 server half: the import lands as a run of segment changesets; recovery and rebuild read the
  segment records, 0 payload reads; imports stream). main confirmed. The push memory work went to w-perf whole;
  t_a752 is w-bugs'.
- C1 step 1 (main approved 08:45, inbox/main-from-w-hg-c1-step1-design.md, with the pull condition): getbundle's file
  revisions come from the changeset store through w-perf's render::StoreWants (Want::Rows via texts_by_tool,
  Want::Chunks via places), answered by hg_view::answer_store_wants between turns (vcs_http/hg.rs:653,
  vcs_ssh/hg.rs:184).
  - A stored delta is sent as it stands when its base is known by node (a revision sent earlier in this answer, or p1
    or p2 whose link is served). Ask rows for (path, [node, p1, p2]).
  - Otherwise send a whole text, made by the store (`StoreHandle::text`), against null: a new Want variant beside
    w-perf's; tell w-perf.
  - Counters: whole texts made; revisions sent whole.
  - main wants a measurement of a one-changeset pull touching a large file, slates' bytes against stock's.
  - Plan:
    - 1a: the by-links walk (clone): Walk::file in serve/getbundle.rs, plus Producing::Bundle's StoreWants in
      serve/session.rs.
    - 1b: the pull path (named_changegroup, built whole and synchronous today) rebuilt as a stepwise walk with wants.
    - Then lookup's .hgtags, the intake's base text, made changesets.
    - Then step 2, after w-perf's intake change lands.
- 09:00 main approved: made changesets onto the store FIRST, then 1a (the clone walk), then 1b (the pull).
  Conditions:
  - ONE HOOK: made_commit (A-804/A-805, Lineage::preview), the one svn_made uses. Ask w-rewrite-ops if it lacks
    anything.
  - ONE PATH: the server-built changegroup lands through the push's changeset path (Record v6, Payload, Texts, v7
    reference). In the same series, remove the v3 made record (record::write_made, the DELTA_VERSION reader/writer, the
    `formed` code), hg_made::recorded, and HgStore::unstored, with no fallback.
  - Red: a repository with a made changeset is refused by the store rule today. Green: it clones and pulls through
    the store, stock 7.2 verifies clean, and a restart replays it.
  PLAN:
  - M1 (slates-hg):
    - record: `write_made_record(room, version, (green, ver))`, a SECTIONED head with Source::Made. head() lets
      SECTIONED/REFERENCE take Push|Made.
    - begin_sectioned keeps the made (green, ver); Retaken finish sets Replayed.made = (green, ver, the first new
      changeset's node).
    - Reference gains `made: Option<(u64, u64)>` (write_made_reference; reference_of; record() picks the made record).
    - `push::made` encodes a Verified as a cg3 changegroup (put_revision_whole per revision; groups changelog,
      manifests, empty tree list, files).
  - M2 (server):
    - made_commit::defer passes `judged` to hg_made::make_deferred.
    - hg_made builds the Verified from a preview (svn_made::revision_held's pattern; changeset_of takes the engine), and
      the bare make() spawns a task as svn_made::make does.
    - The task: encode the cg, take it through the push intake into a fork (hg_push's begin_intake/take path),
      hg_push::changeset_of with the made record, `disk.land` awaited, then made_commit::commit with landed_on_disk,
      the register entry (v7 made reference), the fork placed, and the made map set.
    - The recovery/rebuild replay of v7 already goes through the pieces; replayed_hg sets `made`.
    - Tests poll for the made changeset (async now).
- ~10:10 LANDED ac25475c1: made changesets on the store (one hook, one path). The v3 made record, `unstored` and
  `stored_logs` are removed. Close note written; main and w-perf told.
- NEXT: C1 step 1a, the clone walk (by_links) in serve/getbundle.rs Walk::file through StoreWants.
  - Walk fields: want, wanted logs, the ahead deque (path, position, node, link, row, chunk), and a sent map
    (identity -> node) for the open group.
  - A new `Store::file_entry(path, node) -> ([Node;2], flags)`, read from the index, no text.
  - A stored delta is sent as it stands (put_delta against the sent node, or NULL for a stored whole).
  - Producing::Bundle forwards wants to Answering, then the Walk.
  - The edges already answer wants between turns (vcs_http/hg.rs:653, vcs_ssh/hg.rs:184).
  - Whole callers (getbundle::changegroup, session::serve whole, the tests) need a want-answerer: render's StoredSource
    / answer_want style.
  - Red: a full clone makes no whole text and reads chunks = file revisions; stock 7.2's clone verifies clean.
  - Then 1b (the pull path, named_changegroup) with main's condition: the base on the client or sent earlier, from the
    common set; count whole sends; measure a one-changeset pull touching a big file against stock.
- ~11:20 LANDED 5d2940ff9: C1 step 1a.
  - The by-links walk (serve/getbundle.rs Walk::file) reads file revisions through StoreWants (Files: ask_rows /
    ask_chunks / give) and sends stored deltas as they stand.
  - Store::file_entry and Store::stored_now (test stand-ins; the daemon's Indexed answers nothing at once, and its
    test seam is `answering_from_its_texts`, cfg(test)).
  - HgStores::answer_wants: ServedViews overrides it to hg_view::answer_store_wants, and the default answers from the
    lent view's stored_now.
  - test_store::answer_wants_blocking.
  - hg_interop's stream test does a plain clone that asks the store.
  - Target trimmed for main (27 GB free).
- NEXT 1b, the pull path:
  - named_changegroup (getbundle.rs ~910: common_ancestors, changelog_entries, named_revisions, encode_changegroup) is
    built whole at begin from content-store texts.
  - Plan: keep the planning (which revisions, by path, node, parents, link) but send each file revision from stored
    rows and chunks, stepwise.
  - Ask rows for (path, [node, p1, p2]). The base is sendable if the stored base identity is a revision sent earlier in
    the group, or p1/p2 when the parent's link is held (on the client) or sent.
  - Otherwise send a whole text: a new Want::Texts variant answered by StoreHandle::text pieces (render.rs Want and
    answer_store_wants are w-perf's: tell them), counted WHOLE_SENT.
  - Measure a one-changeset pull of a large file, slates' bytes against stock (main asked).
- LANDED 1b b9fed44e9 (base 5d2940ff9): the pull's file section is serve::getbundle::Pull, read a step at a time from
  the changeset store (rows, then stored edits, then Want::Texts one at a time). A stored delta goes where its base is
  earlier in its group or a parent the common set holds; otherwise the whole text against null, counted
  (Walk::whole_sent, vcs_store.hg_pull_whole_texts). Measured: a one-changeset pull of a 2 MB file is 554 bytes, the
  same as stock 7.2, none whole. Main confirmed; w-lint runs clippy and w-interop the full hg_interop.
- WAITING:
  - main's ruling on lookup's .hgtags: A, wants at each of the four entry points including the --from verb as a task
    (my lean); B, all .hgtags texts kept in RAM; C, .hgtags exempt from step 2;
  - w-perf's answer on intake.rs ownership for the intake's base text (inbox/w-perf-from-w-hg-intake-base.md).
- Target trimmed by exact paths (4 stale rlibs and .o files, 6.9 to 6.2 GB).

## 2026-11-22 ~05:00 PST (w-hg, main 01a20182)
Landed today: aff81b23b (daemon-edge reds, Works::Built uses), d93f98369 (C1 2a read-through, NotYet), 14b00683a,
ed5c20b9d (2b part 2a: away windows ride the image), e765b8245 (LengthDiffers), 2d1399d2c (PastAdmission part 1,
retakeable intake), ecf8840d7 (part 2, RoomWait), 858908e2c (wall), b624f67d6 (part 3 (a), known need admitted),
dc1a69fa0 (wall for (a)).
In flight, ruled (b1) by main 04:44: the push lands as a run of segment changesets as the import does (segments >=
hg_import::segment_bytes, ordinary bounded landings between the intake's turns; entry names the run, v9; made path
keeps v7; conditions: space of an abandoned run comes back, crash oracle at each segment boundary, admission = a step +
one segment, peak RSS before/after).
Held as patches (not landed): ~/.cache/w-hg-2b-1-works.patch (2b (1): the import's bookmark works built a unit a turn
before its landing; green; owes its non-vacuity test that the landing took more than one turn);
~/.cache/w-hg-2b-letgo.patch (2b: the import lets each segment's file windows go once durable; needs 2b (1) first).
Not reproduced, logged: CI's cold-repository "push again" (log line "a push's increment held ahead was refused by the
content store"), a wait's give-up ("a push's intake waited for room and gave up").

## 2026-11-22 ~11:20 PST (w-hg, main 01a20182)
Landed since 05:00: 47edbe430 (take_sliced split, intake cutting, widest_full_text), faa87e583 (v9 push run reference,
wall sites), b61a5934f/746f4753f ((b1) segment runs + (i) landing turn; Rebasing removed; crash oracle; head-of-line
537 ms measured), 89d75e30b (manifest entries kept once; bound linear), 5c15d3e8a (v10 push segment record, base by
node, replay over a moved index).
Now (main 10:40, the spin before staging): stream row gathering sliced a content window a step (render::rows_held;
red a_streams_ask_for_a_long_logs_rows_is_gathered_a_window_a_step proven; hg 416/416; not landed, server tests next).
Running: daemon_edge diag_the_longest_step_of_each_read_a_clone_of_a_long_history_makes at 57,054 changesets (debug).
Suspects next: heads/branchmap (one step over every revision), getbundle named_changegroup (clone -r, every missing
changeset's entry at once).
Then: staging pieces (bounded stale re-base; store staging key kind; forgetting an abandoned key's rows), and a push's
texts let go once their segment lands (RSS 5.5 GB; asked main for content_range.*.resident after push-all).
Held as patches still: ~/.cache/w-hg-2b-1-works.patch, ~/.cache/w-hg-2b-letgo.patch.

## 2026-11-22 ~17:55 PST (w-hg, main 01a20182)
Landed since 11:20: 7ced987e6 (clone spins: getbundle::Named a unit a step; stream ask gathered a window a step),
e72a3d97f (stale re-base equals v10 replay), c2ef17856 (store: forget an unnamed changeset's rows, K keeper rows),
37efbdab6 (staging: segments off the turn, v11 ranges, bounded re-base, abandoned runs forgotten), 62d01f9a8 (RSS:
landed push's file texts let go, bring-back within budget; v9 push reader dropped), 50f62ead2 (two load-sensitive
staging tests bounded by what they measure).
CI 45065911852: 24/24 on three legs, clone spins gone. RSS: arena 4.7 GB held after push-all (texts); let-go landed,
main reruns. Open for main's ruling: landed_pin keeps 1/shards of texts (all on a one-shard node).
Next: 2b (1) (~/.cache/w-hg-2b-1-works.patch, owes the >1-turn non-vacuity test), then 2b let-go for the import, then
changelog/manifest off the content store.

## 2026-11-22 ~19:40 PST (w-hg, main's ruling of 18:43; w-shared's seam 1)
Landed ee1c9005b (piece 23e2789e7): `hg_push::bring_back(repository, (path, node)) -> bool`, the hg half of 2b(b),
part 1. It reads the row, asks the changeset store for the text off the step (`slates_hg::push::held::TextAsk`, the
work builder's ask moved there), and puts it back within the lent room. Counter `vcs.hg.landed_texts_brought_back`.
Test a_landed_file_revisions_let_go_windows_are_brought_back_whole_by_its_path_and_node: red via a stub (49/120 let
go, none back), green. Marked `cfg_attr(not(test), expect(dead_code))` until homes' Hg arm calls it.
w-shared's types landed fc42f581c (note_landed, Landed::Hg, HgLanded).
Next, in main's order (18:5x):
(1) After w-shared's region-object hold fix: hg's red at the edge. A landing whose file shares a window with an
object another repository holds must have its release free only what the push owns (codex-review's
region-object-pin note).
(2) Then seam part 2, in one landing: note_landed per window (object_layout) in let_go_windows, plus the Landed::Hg
arm in homes::bring_back_landed awaiting bring_back per distinct key. Drop the expect. The let-go reds flip to every
window let go.
(3) Then the changelog/manifest move off the content store; main reruns the arena lane after the pin goes.

## 2026-11-22 ~21:30 PST (w-hg)
Landed 6b36e0269. Two pieces, each red-first:
- 139fc4e9f, the intake's refused finish (main's routing of w-svn's sweep). A refused node check or finish_object
  dropped the writer with its windows held; both now abandon it. Red (true, true, 2), now none held.
- a401e9c51 plus the WIP commit before it, seam part 2 (hg's half of 2b(b)).
  - Segments carry FileContents (path, node, ids).
  - The let-go after a push's entry commits or an import lands notes each window as Landed::Hg and lets it go,
    pinned by nothing.
  - The let-go at segment durability notes nothing: bring_back reads the committed index.
  - homes::bring_back_landed has its Hg arm.
  - Test-only: Works::Whole replaced by plan, then works_whole (blocking read-through), then Works::Built.
    prepare takes the view alone.
  - Red-first edge test: a region object sharing a landed file's window is read whole from its home.
  - The let-go tests flip to every window let go (were 71/120 and 1/5 pinned).
  - Focused: server 77/77, hg 133/133.
Next: main reruns the arena lane (the pin is gone), then the changelog/manifest move (replay and intake first).
Main's heavier asks: CI hg matrix plus the arena diag at 6b36e0269 (push-all arena and RSS; landed_texts_brought_back).

## 2026-11-22 ~22:00 PST: plan to the bar (Ada via main): peak RSS ~410 MiB on the 57,054-changeset matrix
Numbers are from CI 45101827617 (before 2b(b); arm64, after push-all) unless marked "est." (my estimate, to be
replaced by arena-3's read at 6b36e0269). Each term is held at the peak; the budget share is what it must fit.
Ordered by bytes now:
| # | Term | Now | Share of 410 MiB | Piece |
|---|------|-----|------------------|-------|
| 1 | Content arena, shard 0 (file texts pinned, plus index rows, changelog, manifests) | 2,535 MiB allocated | <=120 MiB | 2b(b) landed: pinned texts go after commit. Remainder = rows + changelog + manifests (terms 4-6) |
| 2 | Allocator slack: heap freed but resident, arena mappings reserved | heap 1.0 GB reserved / 435 MB used (~600 MB slack); 41 arena mappings 2.89 GB reserved; idle RSS 3,742 MiB | <=40 MiB | Give pages back when a push, clone or import ends (the allocator's purge / decommit of empty arena runs). Needs main's read of resident vs allocated per mapping |
| 3 | Heap in use, unattributed | 435 MB | <=80 MiB | Attribute first (main's heap profile at the peak and after push-all), then cut whatever dominates |
| 4 | Intake per-push structures, whole push (Run.contents FileContents ~112 B + path per file revision; Taker brought/awaited/positions/manifest_of; Intaken changesets/logs) | est. ~100+ MB in push-all | <=30 MiB, bounded by a segment | Bound per segment, not history: no FileContents list (walk the fork's file rows at commit to note); awaited checked a window a step against the fork; brought/positions shed as segments land |
| 5 | Manifests in the content store | est. 50-300 MB | <=60 MiB | Changelog/manifest move off the content store (replay, then intake, then works/made/verify, then getbundle, then kept_as/Rebase), only if arena-3 shows they dominate term 1's remainder |
| 6 | Index rows and runs (all logs' rows in content store windows) | est. ~60 MB (~150 B a row) | <=60 MiB | Measure; if over, page cold rows out to the store, as texts are |
| 7 | Changelog texts | est. ~25 MB | in 5's share | With 5 |
| 8 | Segment working set (payload slice + its texts) | one segment_bytes, est. a few MB | <=10 MiB | Already bounded by segment_bytes |
| 9 | Clone encoding (getbundle Named a unit a step; stream rows_a_step) | est. <10 MB | <=10 MiB | Already bounded a step; confirm the peak during clone rows |
Order of work: arena-3 read -> 2 (slack, the largest lever after 2b(b)) and 3 (attribution), with main's measurements -> 4 (push bounded by its work) -> 5/7 if they dominate -> 6.
Flake (main's 22:00): a_cleanly_restarted_import_brings_back_no_named_text. checkpoint_green defers to a
checkpoint_in_turns task under load and the test stops first, so the pre-pass rightly brings back 13. A test/doc fix;
sent to main and w-svn.

## 2026-11-22 ~23:30 PST (w-hg, main's URGENT on arena-3 run 45110563116)
- Landed c01aac8f8 (merge 18983677e): the commit-time noting and let-go is taken a turn at a time
  (hg_run::let_go_committed, half the step quantum, a revision its unit). Push: after its entry commits. Import: after
  its landing is durable. Red: 1 turn of 642,500 ns, now 120. Caveat: one revision (40-63 us debug) exceeds the 6 us
  quantum, so a turn is half the quantum plus one revision.
- Landed 29561c25e: store keeper_key by checked splits; EntriesOf alias (lint wall at dc549ba48).
- Open (sent to main and w-shared): the restart spin (serve_loop poll 2, 1,040 ms). Lead: vcs_home_pieces and
  restore_landed are O(notes) in one call.
- Next: the memory plan when arena-4 lands (w-shared's terms: the push peak is the rendezvous pin until commit, and
  ~1.3 GB stored after the push is unexplained).

## 2026-11-23 ~00:10 PST (w-hg)
Landed 6b7a4982f: a landed hg window is noted only where landed_pin would pin it (held for the region), per w-shared's
1ecb57cf4 (rendezvous pin gone) and main's ruling. Red: the let-go tests assert no notes (every window was noted).
9f42d8942: unused test bindings dropped. w-shared told of its 3 warnings (daemon.rs:8215/8256, homes.rs:75).
Waiting: arena-4 (1baba966b) rows from main, then the 410 MiB plan.

## 2026-11-23 ~01:30 PST (w-hg, arena-4 CI 45117277400)
- Landed d0fe61d57 (merge e7e0e4fa3): getbundle's file sections (clone link walk Files::ask_rows; pull Pull::ask_rows)
  ask at most Store::rows_a_step rows a step; the link walk counts rows scanned. That was arm64's 2 s step. Red: one
  ask of 40 and of 78 rows, now within 6, changegroups unchanged.
- Plan order agreed with main: the push-all peak first, judged by the push-all row's VmHWM, with
  vcs.hg.push_held_high_bytes as the non-vacuity gauge; a test drives two histories of different lengths and asserts
  the same high-water within a segment's slack.
- Next piece: that gauge, then bound the push's per-history structures (FileContents list, Taker brought/awaited/
  positions/manifest_of, Intaken changesets/logs) by the segment.

## 2026-11-23 ~02:40 PST (w-hg)
Landed a71609a9f:
- e21e594c9: store keepers_of reads by checked chunks (lint wall 8e5ea5489, texts.rs:576).
- e4d95bcf9: push gauges vcs.hg.push_held_high_bytes / _texts_high_bytes / push_rows_bytes. Two-history test (60 vs 240
  changesets): texts 32,768 both, rows 35,860 vs 143,780, held 89,577 vs 241,536. The admission grows ~843 B a
  changeset (bookkeeping kept to the push's end): the next piece's red ("held equal within a segment").
Waiting: arena-5 VmHWM. If the bookkeeping is next: bound it by the segment (FileContents list, awaited, brought,
positions, manifest_of, Intaken), so held is equal within a segment.

## 2026-11-23 ~05:00 PST (w-hg, arena-5 CI 45127535742: peak 2.1 GB, floor 1.73 GB mostly allocator retention, 57 GB churn)
Landed 4eeb99b67 (piece 1dc65ad07): a push's churn per changeset is one manifest copy (was 3.20 at a 91 KB manifest).
kept_as is given the text (append_revisions_after's `given`); encode_lines walks by cursors. Test:
crates/hg/tests/push_heap.rs (LargeMapped plus an arena storage like the daemon's; heap() readings' own cost taken off).
Gotchas for the next person: heap() allocates a residency buffer for every mapped block; HeapWindows puts windows on the
system heap, the daemon's arena does not.
Next: the x86 348 ms single step (wrote=43); branchmap whole 155 ms cpu (x86) later.

## 2026-11-23 ~07:30 PST (w-hg)
- The x86 348 ms step (wrote=43) was a `lookup` of a branch name: branch_tip scanned every revision's branch, then
  branch_head_revs walked the whole history (= branchmap whole, 155-161 ms on x86).
- Landed fa14530ba (main's ruling (b)): HgIndex keeps branch heads (index::BranchHeads), updated in
  append_changesets. Views serving the whole changelog answer from them; others walk (walked_branch_head_revs is the
  oracle). Tests: proptest oracle over grouped appends; lookup and branchmap on a long line read ≤ 8 (red: 512/511).
- Open: views with left-out changesets still walk; a cross-branch merge's uncertain walk is bounded only by the
  branch's lowest head.
Waiting: arena-6 (churn) and arena-7 (lookup/branchmap times) from main.

## 2026-11-23 ~08:30 PST (w-hg)
- Landed 2b6c49e6b: BranchHeads::longest_walk plus vcs.hg.branch_heads_longest_walk (a high-water at each index swap).
  Test: a 400-revision line taken across is walked in one append and counted. Slice it if the arena shows walks past
  a step's worth.
- The left-out view (secret or hidden) isn't reachable from the hg matrix: no evolution, pushes public, no import.
Waiting: arena-6 and arena-7.
- 83f1d4c00: GAPS records changeset evolution as the next completeness item: an evolution-enabled matrix row (amend
  and rebase pushed with markers), the hidden view served, its branch heads kept. It's next after arena-6 and arena-7,
  unless they name a memory or time item.

## 2026-11-23 ~10:00 PST (w-hg, arena-6 CI 45136304371: churn 45.05 GB from 57.3; peak and floor unchanged)
- Intake churn by site (throwaway sampling probe, 2,000-file manifest, 1,501 changesets): ~166 KB a changeset.
  delta::apply 84% (the one manifest copy); index key building 5%; Placed growth 1.6%; Planner 1.6%; records 0.5%.
- So the intake is ~6 GB of push-all at hg's sizes; most of the 45 GB is server-side or in the clone/stream/verify
  rows. Proposed to main: a server-level counting binary (crates/server/tests/hg_push_heap.rs) to name it, then the
  cuts by size, including the manifest buffer reuse (apply into the previous manifest's Vec, swap).
Waiting: main's order, arena-7 and arena-8 (MALLOC_ARENA_MAX=1 test of retention).

## 2026-11-23 ~11:30 PST (w-hg)
- Landed a4debd938 (merge df71a0ed5): the intake rebuilds a manifest into the previous one's buffer (Taker::spare).
  push_heap: 102,045 down to 10,766 B an edit at 1,600 files.
- Server heap binary: asked main to choose a route. (A) an integration test with an anchor/issuer/store harness and real
  hg subprocesses; (B) a lib test with a cfg(test) global allocator; (C) a sampling allocator in cli main on a diag
  branch over the real matrix. I recommended C for the numbers, then A as the lasting test.
Waiting: main's route; arena-7 and arena-8.

## 2026-11-23 ~12:30 PST (w-hg)
- (C) is ready: branch w-hg-diag/alloc-sites at e3bbd2cc8 (DIAG only, not run; main pushes it as arena-9). It adds an
  env-gated sampling allocator in cli main.rs with stamped cumulative top-site dumps, ROW-START stamps in
  hg-matrix.sh, and a line-tables build.
- Process slip: used `git checkout -b` in w-hg-a712, against the no-checkout rule; nothing was lost. Next work goes in
  the new worktree ~/slates-work/w-hg-b (agents/w-hg-b, from origin/main via `git worktree add`). w-hg-a712 is left on
  the diag branch.
- Next: (A), the server integration harness (crates/server/tests/hg_push_heap.rs): an in-process Daemon::start with
  a store owner, an issuer-minted token, create_repository, and real hg subprocesses (debugbuilddag history; push,
  clone, clone --stream, verify), skipping loudly without hg.

## 2026-11-23 ~14:00 PST (w-hg, worktree ~/slates-work/w-hg-b)
- Landed 9c8e4a95e: crates/server/tests/hg_push_heap.rs, an in-process daemon under real hg subprocesses (push, clone,
  stream, verify) with a byte sampler, top sites per row.
- First read: push 702 KB a changeset (HgExchange 21.7%, watchdog p99 14.4% plus its beat 9.5%, phase_heads 8.8%,
  ChunkWindows::put 4.6%); clone and stream about 26 KB a changeset.
- Flagged to main: the watchdog's per-beat allocation (several GB over a push-all); push::phase_heads collected per
  step; the HTTP exchange's buffering.
- Next (unless main routes otherwise): phase_heads and the HTTP exchange site.

## 2026-11-23 ~15:30 PST (w-hg, worktree w-hg-b)
- Landed dcf71036e: six R3 literals. The record versions end with a Format: line; homes' 16/20 are named; the test
  hook's 1 ms poll is replaced by a waker (release_parked).
- Landed f14ea81bb: the heap sampler no longer counts its own allocations (a thread's first symbolization was being
  charged to its next allocation); its counter is per thread. Corrected push: 362 KB a changeset, 86% Greens::lend
  (Green::default's 2 KiB SetHash plus a Box per lend). phase_heads and the watchdog were artifacts.
- Told main arena-9's diag sampler shares the guard flaw; it's minor at full scale.
- Asked main whether I take the greens lend cut or route it. Next: (c) the HgExchange site (likely also an artifact),
  then lookup's `prefixed` 91 ms begin.

## 2026-11-23 ~17:00 PST (w-hg, worktree w-hg-b)
- Landed 1af2b501d (merge 38001e7dc): a lineage member's lend allocates nothing (lazy SetHash lanes; Lent in a
  Lineage::lent slot, no Box). hg_push_heap's push went from 362,278 to 49,909 B a changeset; the test asserts lend
  bytes are 0.
- Pre-existing failure on main (w-svn's), reported to main: svn_commit
  a_revision_refused_at_publication_leaves_the_green_where_the_log_is_and_the_next_lands.
- Next: lookup's 91 ms `prefixed` begin (a node-prefix index, or the lookup's own slices).
- Landed d55c060b3: a node prefix's lookup is read by node from the index (HgIndex::changesets_prefixed), with no
  hex walk. Oracle against the walk; at most 8 reads.
- Queue clear. Next: changeset evolution (an evolution row, the hidden view, its kept heads), unless arena-10 names a
  memory item.
- Landed 8dd4adbb5 (evolution piece 1, be9ffd98f): pushed obsolescence markers are kept and served. The new
  hg_evolution test runs real hg with evolution = all, pushes an amend and a rebase, and clones: the clone holds the
  2 markers and the successors. A marker on public a is refused in 7.2's createmarkers words. The real-hg harness is
  tests/common/hg.rs. Flaky under a parallel run: t_goal93 landed-pack let-go (git; passed 3/3 alone).
- Next: kept branch heads for a view that leaves changesets out. Still owed: non-publishing repos and the hidden
  view, rewrite admission, the obsolete pushkey namespace, the "N new" output.
- Landed b36b2e9c9 (evolution piece 2, cd7b1b569): a view that leaves changesets out gets its branch heads from the
  kept ones, with no walk. BranchHeads logs each append's endings in shared Arc chunks (8 B an ending), and
  BranchHeads::served reads back to `first`, then replays the served tail. A property test (3 branches, every phase,
  prunes and rewrites) equals the walk; it was red before. Also a 9,000-changeset test past sealed chunks.
- Next: non-publishing repositories (phases.publish from the record), so the hidden view is exercised end to end
  with real hg. Then decision 3's rewrite admission.
- Landed ec92899f5: the Arc fix (R2). BranchHeads' endings are now sealed into index rows ('E' keys); a fork shares
  them by table identity and copies only the open chunk (512 endings or fewer). note's walk is split out (lint :469).
- Marker tuple piece committed (c3d3c18f2) and landing: a format 0 marker made every clone fail; golden vectors from
  7.2's writers; markers compared as 7.2's tuples.
- hg_publishing in progress (w-hg-arc): RepositoryPolicy field (manual Default true), DatabaseRefusal 14
  PolicyPublishingNotMercurial (show main the wire_order diff before landing), CLI --hg-publish, view/push read it.
  Red: hg_evolution a_non_publishing_repository_keeps_drafts_and_hides_a_rewritten_one.
- Agreed split with w-perf for glibc's term. w-perf takes the store's TextCache. I take intake::rebuilt,
  HgIndex::revision_text (with object_bytes) and Taker::file_base. Repro: SLATES_HG_PUSH_SOURCE=$HOME/.cache/w-perf/hg-src
  CARGO_PROFILE_DEV_OPT_LEVEL=2 CARGO_INCREMENTAL=0 cargo test -p slates-server --test hg_push_lifetimes -- --ignored
  --nocapture (test a_pushs_allocation_by_site_size_and_life_under_stock_mercurial).
- Landed c3d3c18f2 (marker tuples; format 0 markers clone again) and bff41b98e (hg_publishing: RepositoryPolicy
  field, DatabaseRefusal 14, CLI --hg-publish; real-hg non-publishing test, red under the const "cannot amend public
  changesets"). Index-tail seal handed to w-perf (one site in HgIndex::write).
- New rule from main: clippy -p <crate> --lib -D warnings on any new external-bytes parser before landing.
- Evolution owed: decision 3's rewrite admission, the obsolete pushkey namespace (legacy). Next: the C1 cuts
  (intake::rebuilt, revision_text with object_bytes, file_base), unless main reorders.
- Landed f02fa1a28 (C1 cut 1: intake file texts, 1.76 -> 0.74 copies at 16 KiB, 1.08 -> 0.08 at 160 KiB). arena-15
  (main 45de607e7) passes push-all, RSS 1,220 MB (was 1,865 and 2,235), heap live 230 MB (was 399).
- Cut 2 (9003d110e: revision_text through the index's own buffers; manifest read 2.13 -> 1.16 copies) is landing
  under main's push-race rule: after green, re-merge, and push at once unless hg paths changed. Merges kept
  `buffers` against w-perf's seal (reverted in 2662ca69f).
- Order (main): cut 2, cut 3 (file_base borrows from the cache; red: deltas on the revision two back), then the index
  seal's reland, which is mine. The design main ruled:
  - Seal when due in one insert helper after insert_all, as 0cd8ef36a did. Import's settle seal removed. 2662ca69f's
    revert shows exactly what to put back.
  - Bound the fork's runs with one merge path, index_upkeep's merge_step, sliced within the step's budget (the loop
    is TakingIntake::slice in hg_push.rs, around line 2286; the fork lives in Taker.fork).
  - The rule is size-tiered: merge the newest own runs while the newer holds at least half the older's rows. Runs
    number at most log2(rows / rows a table) + 1, on a Derived: line, beside the rent-or-buy compaction_due.
  - Red first on two measures: runs walked per lookup held under the bound across a long push, and one segment step
    within the quantum. Then measure the 56 MB tail's cut; main lands it with arena-16.
- w-perf now takes the content arena's page rounding (a small-window slab; the 556 MB is 114,200 windows of about
  1,151 B, each on its own 4 KiB page).
  - w-perf's two points: the Option<Merge> lives in the Intake beside Taker.fork, as a step kind run within
    TakingIntake::slice's pacing (a refused or abandoned push drops it with the fork). I add the tiered rule in
    crates/vcs/src/index.rs myself; w-perf stays in vfs/content.rs and arena_windows.rs.
- Landed 9003d110e (C1 cut 2), e4cad14ad (C1 cut 3: a cached base read where it lies, let go after its last user;
  1.08 -> 0.08 copies), 0396009b9 (Windows server tests compile: the hg harness's principal is channel_account), and
  c6fab004e (the seal's reland: HgIndex::insert seals when due, Index::tiered_due merges a push fork's runs a table a
  step; counters vcs.hg.push_most_own_runs and vcs.hg.push_tiered_merges for arena-18).
- arena-17 (cuts 2 and 3 plus w-perf's slab): RSS after push-all about 797-826 MB, down a third. The goal is about
  410 MiB.
- Next: decision 3's rewrite admission (evolution's last item), unless main reorders.
- Landed cdfeae794 (evolution decision 3: a rewrite moves its line only when it drops obsolete drafts alone;
  evolution is done, per main) and c9a39f7d2 (Intake::unit and merging, plus the intake_steps example, which times
  steps by kind).
- A/B (local, release, at main's direction before CLAUDE.md §8 was re-read): at 57k, C (seal + tiered merge)
  1,183.6 s against A (no seal) 1,188.6 s, merges 5.29 s. So the reland costs the intake nothing; arena-18's slowdown
  is outside it, or another commit (main's arena-19 decides). Synthetic: C 20.5/18.0 s, B 30.0/25.0 s,
  A 18.9/15.8 s.
- Per §8, measurement is main's. The 57k step-kind attribution is a run request to main (exact command in the
  message). Waiting for: its reading (then fix the biggest long-step kind red first, and restate the bound with the
  seals one merge spans), and arena-19 (if the reland costs time outside the intake: reads of sealed runs, fork fence
  copies, the store's own merge).

## 2026-11-23: push counters for arena-20 landed (fa2d728f2, merge c876434a1)
- New counters: vcs.hg.push_index_lookups and push_index_runs_visited (counted at settle, from the fork's Index counters); vcs.hg.fork_fences_copied (counted per held generation).
- Told Slates the DIAG names, plus the existing landed_texts_brought_back, hg_push.intake_slices and vcs.hg.segment_record_turns.
- Hypothesis: merge_step's last step does the whole merge's landing at once (puts every table, takes their holds, releases the merged runs); r499's 89.7 ms step.
- Proposed piece: put each table as it is cut. Waiting on r536 and main's ruling.
- Still to do: (B) the tiered rule and bound stated in bytes; then optimize the pending steps (72% of intake time).

## 2026-11-23: merge-step slicing landed (8933770f2, merge 97caa43f1)
- Each merge step now puts one table or gives one back. The index holds a merge's work between steps; a given-up, refused or dropped merge gives back what it put.
- The upkeep lends one table and its fence per step (Merge::step_room). merge_begin now returns Result<Option<Merge>>.
- Red: no_merge_step_puts_more_than_one_table failed "step 8 put 7 tables" on the old code.
- r536 (main, 19:10): the reland costs +20% under load (84 s against 70 s). The cost is Index::get walking sealed runs, each run resolved through ContentStore::get and the shared object's atomics.
- Next: cut resolves per lookup. Red: a test counting resolves per lookup. Proposed fix: a per-run key filter. Asked main to choose between it and (B).
- Still to do: (B) the bound in bytes; then the pending steps (72% of intake time).

## 2026-11-23: a key filter per run landed (fa30061d2, merge 34d055a3c)
- Each run keeps a Bloom filter of its keys (vcs/src/filter.rs), sized by a derived rule: k = ceil(log2 2R) probes, with R = 2(floor(log3 budget)+1). A fork's copies of shared runs carry no filter.
- Red: 2,000 misses over 5 runs fetched 9,502 tables before, 95 after; 9,903 runs passed over by the filters.
- New counters: vcs.hg.push_index_resolves and push_index_filter_rejections.
- Asked main for the r536 loaded matrix at 34d055a3c.
- Next: (B), the tiered bound in bytes. Possible follow-up: give a push's fork the parent's filters.

## 2026-11-23: (B) landed: the tiered rule weighs bytes (40991f613, merge f9a5539b5)
- tiered_due now merges by tables' bytes. TIER_RATIO and TIER_BASE are one Derived decision; the key filter's sizing reads TIER_BASE. New Index::own_bytes.
- Red: 8 own runs against a bytes bound of 5 under the old rule in rows.
- intake_steps prints own bytes, windows and the bound. Asked main for the 57k run at f9a5539b5.
- Next: optimize the pending steps (72% of intake, 48,795 steps at 1.56 ms in r499). arena-21's resolves decide the parent's-filters follow-up.

## 2026-11-23 ~21:45: C1 (1) P1 and the tiered fix landed
- 925a1ecaf: intake_steps times pending-last apart. r618 found pending-last is 51% of the wall.
- C1's rest is not whole (main's 20:49 order): (1) changelog and manifest texts off the content store, then (2) the intake keeping no whole text.
  - (1) is in five pieces: P1 primitives, P2 serving, P3 Held/work, P4 intake/record, P5 switch on the let-go plus main's red. No pinned manifest: the learned cache only, with asks per push counted.
  - P1 landed as 95a6be82f (merge 261c8b693): log_text_held, bring_back_log, and hg_push::bring_back for \0changelog and \0manifest; counter landed_log_texts_brought_back.
- Tiered fix landed as c81dce7db (merge 14b44fb7f): merge through the deepest violation; most_own_runs sampled at the invariant point, plus _in_flight. r618 had 8 own runs against a bound of 6.
- Before P2, merge w-tsan's lend fix (within_budget outside lent_state). bring_back_log is the fourth site.
- After C1: the interop speed round, stream (8.7x slower than stock) then push (6x slower). Then the pending steps.

## 2026-11-23 22:08: queued correctness item (main, from w-svn's sweep; svn's A-888 piece 2)
- In hg_made.rs a version can be committed with nothing behind it on the store (the green ends up ahead of the store). Sites:
  - made Err or Ok(None) commits the version alone;
  - Unmade other than Unlanded commits it alone;
  - commit_made Err commits the version through made_commit::commit_held;
  - hg_made::make on the bare path records in the same transaction, counted when unmade.
- Use w-svn's guard-then-apply for commit_held when it lands. Red first per site where the path can be driven.
- Order: after C1 P2, before the stream and push speed items.
- Waiting: w-tsan's lend fix (merge it before P2); r670, the 57k rerun at 14b44fb7f.

## 2026-11-24 ~03:30: C1 (1) P2, P3, P4 and the status sweep landed
- P2 249865667: a fetch's changeset and manifest units, and lookup's .hgtags, ask the store for a let-go text.
- P3 5de28ccb2: a push's work lays out again after bringing back a NotAtHand changeset or manifest. bring_back_log_on takes forked, placed after w-perf's split (8cd23c967).
- P4 63e25a1ca: the intake pre-checks its delta base and previous manifest and asks the store through Stepped::Store. The end-to-end edge red lands with it.
- Sweep 16186398b: the hg and git push assertions in daemon_edge judge the body. None was passing on status alone.
- Held, not landed:
  - ~/.cache/w-hg-made-reds.patch: the four hg_made commit-alone reds. They land with w-svn's commit_held guard.
- CLOSED by b47217878 (main 5a773a49a), (B'): manifest-chain reach. One rule (`push::manifest_row`) rests a row on the fork's last manifest; the live intake reads that base's windows and asks the store where let go (Stepped::Store); the replay and stale re-base read it over given/brought-back texts (`revision_text_over`), held by a_record_replayed_where_its_content_was_never_kept_asks_the_changeset_store, a_successors_rebuild_lets_go_the_texts_it_brought_back_once_it_lands, a_restarts_replay_lets_go_the_texts_it_brought_back. The let-go of a window a kept revision still names is (B)'s, 4a0c23e20.
- Then: P5 (switch on the let-go, main's red, asks per push counted), then C1 (2), the intake keeping no whole text.
- Then: stream 9.2x and push 5.8x slower than stock; pending-last 51% of intake time.

## 2026-11-24 ~04:00: the reach, (3) and the built_works split landed
- a313f83d9: a stale re-base asks for let-go manifest texts. Guard: a push's segment record reads no text.
- b82590ddf: a replay asks for let-go changeset texts, batched by the record's room, and for manifest texts (own, base, previous). The four server drivers answer HgStep::Asked. Counter vcs_store.hg_replay_texts_asked, for main's restart round.
- 011e8e80e: built_works split (lint, cognitive complexity 14 of 10).
- Not mine, reported: t_goal93_a_recovery_keeps_a_landed_pack_while_the_landed_let_go_is_held (git) fails here 4 of 4.
- Next: P5. Switch on the let-go for changelog and manifest windows after a push commits or an import segment lands. Main's red: ContentStore::used, clone and pull verify. Count asks per push, with the parent manifest's read-back named apart. GAPS entry off IN FLIGHT.
- Held: ~/.cache/w-hg-made-reds.patch, waiting on w-svn's commit_held guard.

## 2026-11-24 ~05:30: P4b and A-891's Mercurial half landed
- 866bfc1dd: P4b. A made changeset brings its parent's texts back before it is made. Fixed with it: a green's second deferred submit was unmade NotHead (the preview read at the green's own version numbers).
- 946484875: hg_made guard-then-apply via commit_guarded and refuse. The bare path is removed (an unrecorded Mercurial submit is refused RecordedSubmitRequired). made_commit::commit removed, agreed with w-git. Sweep listed in the commit.
- The held reds patch is consumed (~/.cache/w-hg-made-reds.patch no longer needed).
- Next: P5. Let go changelog and manifest windows after commit or landing. Main's red: ContentStore::used, plus clone and pull verifying clean against stock 7.2. Asks per push counted. GAPS off IN FLIGHT.
- Then: stream (8.8x) and push (5.6x) speed.

## 2026-11-24 ~11:00: re-adoption NotHead, the empty-window durability fix, and P5 landed
- 259b0db84: a re-adopted lineage root founds its lineage again (`verbs::install_readopted`), so a made changeset over it is previewed. Red: the second edit of `a_readopted_green_brings_back_the_named_base_its_chain_splices_first`. The Branch-member and takeover siblings went to w-perf (main's ruling).
- 53e09728f: the empty window is never let go (`LetGoRefused::Empty`). Red: Mercurial's first 5,000 revisions pushed and restarted refused `Unheld 00manifest rev 4281`, a manifest kept as an empty delta whose empty window an empty file's let-go had marked away with length 0. Main's r942 (57k, big Studio) is green.
- c45aff536: P5. Changelog and manifest texts are let go once landed. Readers (layout, made parent, named resolution) read whole texts given in the objects' place. Asks are counted per push, with the parent manifest named apart. GAPS entry moved off IN FLIGHT.
- Run request (main): stock 7.2 clone, pull and verify over an import and a push (hg_interop).
- Process slip, owned: one bare `git reset -q`, which only unstaged files a `git apply --3way` had staged. Nothing was lost.
- Next (main's order): the `hg_push::landing` poll of 3,553 ms at a 4,067-changeset merge-heavy DAG (debug, exit 4). Red first: a counter names the per-history work, then slice it. Then C1 (2), then stream and push speed.

## 2026-11-24 ~16:00: P5's interop reds and the landing spin landed
- 26d1b3846: a fetch's changelog and manifest texts read whole are counted apart (`vcs_store.hg_pull_log_texts`). main's hg_interop pull red (3 "sent whole") was a miscount; the pull's shape was right all along.
- 9a6e470a8: a parked tag lookup's held read is judged by its heads (`TagsRead::same_heads`). `branch create --from v1` had hung, parked again forever once the heads' manifests were let go.
- a1f055a5e: A-687's served repository reads a let-go changeset text through the changeset store (w-perf's find; a P5 slip my filters missed).
- fda5613a1: the landing spin. A push's new line greens are made a slice at a time before the landing step (`push::prepare_create`, `PreparedCreates`, `hg_push::land_sliced`), agreed with main and w-git. 452 heads: 3.5 s+ watchdog exit → 567 ms step, accepted. 152 heads: 1.89 s → 166 ms.
- Run requests to main: stock 7.2 hg_interop at ffad3c6ae (r981, queued); the 452-head DAG and the 57k history through hg_push_restart in the optimized profile.
- Owed (GAPS): hundreds of existing heads moved in one push; the record's write grows with heads × tree; one green a slice; git's driver adopting PreparedCreates (w-git).
- Process slips, owned: two git resets (one bare, one path-limited), neither touching work; one `cargo build --tests` that filled the shared disk (freed through kiln).
- Next: C1 (2), the intake keeping no whole text; then stream and push speed, then pending steps.

## 2026-11-24 ~20:00: after the reboot, TSan fix and C1 (2) piece 1 landed
- f289d786f: TSan diag 3's parking test read its parked interval before the release begins (the release's round trip had been inside it); an ordering claim, nothing scaled or skipped.
- f754c36f6 + 4885954fa: C1 (2) piece 1. A push's file revision is taken a window a step, never whole (`delta::Patching` over a base read from the fork's rows via `run_at`; a let-go base asked and brought back). The base cache and chain rebuild are gone for files. Red: 43,025 bytes held → ≤ 8,192.
- Reported to main, not mine: the A-908 holder-rebuild test fails 4/5 alone under load (Later vs Installed(1)).
- Run request to main: 57k push+restart and r618 intake_steps in release.
- Next: C1 (2) piece 2, manifests a window at a time (Taker::last, read_of parsing every entry), then changesets.

## 2026-11-24 ~21:30: hg clone regression (P5) fixed and landed
- 4f9438625 (main f8fe97e5e): after P5 a clone asked each let-go changeset and manifest text alone, and the store's `texts_by_tools` scanned the log's tool-id prefix from its start a row a page per ask: quadratic (hg 57k clone past 13.5 min vs stock 42.4 s). Now each scan begins at the next asked id (store red: 200 reads → 1), and the fetch batches its log texts (clone red: 30 store asks → 6).
- Main reruns the interop hg leg at f8fe97e5e.
- Parked: C1 (2) manifests (B) as ~/.cache/w-hg-scratch/c1-2-manifests-wip.patch + chain_read.rs + intake_manifest.rs; its tests red, one hung (a loop). Pure pieces landed: 105609857 (manifest::Checking, delta::Written).
- Next: resume manifests (B), the loop first.

## 2026-11-24 ~23:30: C1 (2) manifests (B') and the write-path leak fixes landed
- b47217878 (main 5a773a49a): manifests taken a window a step; one rule (`push::manifest_row`) for the live intake, segment replay and stale re-base; `manifest_diff::Diffing` (streaming sorted-merge diff, proptest). Red: 43,329 bytes held on main → within 16,528. Also fixed my 4885954fa leak (a let-go base brought back for one revision never let go).
- 3e50c0aee (main 1aafd8f8e): main's three write-path items. hg_push work read-back: not a leak (within_budget step lend; guard added). hg_named partial group: fixed (red). record.rs finish error: not a leak (store released whole).
- Sweep reported to main: svn and git write-path sites routed to their owners; read paths pending w-perf's division ruling.
- Next: hg stream clone (4.7x stock) and the clone's let-go reads (~1.7x beyond load).

## 2026-11-25: main's ruling (B) landed -- the content store's unlanded pins
- 123d8c280 (main 4a0c23e20): `ContentStore::pin_unlanded`/`unpin_unlanded`, a count per window in the window table; every let-go refuses `Unlanded` while it is above zero (counted `vcs_store.content.kept_unlanded`); `pinned_away` counts a window found let go as pinned (must stay 0); NotHeld for a pin of nothing; CountExhausted at width. hg push: pins after each slice, unpins at each segment's let-go (`landed_segment`), the rest at the push's end; `taken_whole` unpins over the state it holds (Drop's nested with_state leaked them). Supersedes 7ad0c7931's PUSH_PINS. Reviewed by w-perf.
- Reviewed w-perf's 142256360 (text_whole; the clone's text-cache walks): OK. Part 2 (batch bound) owed by w-perf; I review.
- Owed to main: the six-push 57k loop (red: any UnknownDeltaBase, or kept_unlanded 0, or pinned_away > 0). w-svn moves svn onto the store's pins.
- Next (main's order): stream v2 negotiated (stream2 capability, requires), then zstd compression and inline small revlogs; check crates/cli/bench/hg_stream_compare.sh (w-interop's b2888af84).
- 1540f62d5 (main 50c31d6d9): stream v2 -- `bundle2` lists `stream=v2`; `getbundle` `stream=1` answers one STREAM2 part (Streaming framed as `_emit2`, counted first; phaseroots for a non-publishing repository). Three red tests. Run request to main: hg_interop stock stream-clone test (now asserts stream2 negotiated).
- Split (main): w-interop owns the encoding (zstd chunks, inline revlogs, requires); I recommended (a) compress at stream time now, (b) store keeps stock's chunk form later as my piece. Review w-interop's diff at the Streaming seam when sent.
- Waiting: main's six-push 57k loop at 4a0c23e20 (B); w-perf's batch-bound part 2 for review.
- Main ruled: (a) stream-time compression now (w-interop, with count/send CPU counters); (b) RULED as my next piece after (a) lands: the store keeps stock's revlog chunk form on arrival ("content once on arrival"), replacing (a)'s per-stream compression; the place contract changes for every reader in one wave (intake bases/chain_read, pull deltas, replay/re-base, bring_back, .hgtags, owner texts). Red first.
- (b) PLAN APPROVED by main (2026-11-25): T row form byte (row encoding v+1; old rows raw; type byte inside typed rows); arrival writes stock chunks (record v+1); one decoder per layer (store read_place, stream copy / old-row encode, fetch file-section decode, replay/re-base); reds per class + r400 vs stock (zstd byte equality only as measured). Conditions: (1) compress at arrival and decompress at read sliced under the step budget via zstd streaming, copy_slice_bytes a step, counted compress_bytes_most / decompress_bytes_most asserted in a red; (2) level 3, 50-byte floor, raw-when-NUL each with /// citing stock's revlog.compress / zstd default, tested vs stock's chunk kinds at r400; (3) fetch decode owner-side vs hg-side by w-perf's step bound, named in the commit. Starts after w-interop's (a) lands.
- Reviewed w-perf's fffd95f79 (batch bound part 2): OK, 4 notes (walk left in flight on Past mid-text; held bytes one text; empty trailing request; lend None ends answer). answer_wants_over sweep left to w-perf.
- Main ran hg_interop stock stream-clone test at d3057763e: passed (stream2 negotiated, no stream_out). Flagged: hg_stream_compare build at 2dd25afe6 predates stream v2 (50c31d6d9).
- 2026-11-25 landed main 62e28d008: 8ec5fb287 (push pins look at a slice's new revisions only; red 37,777 looks → 192; counters hg_push.pin_{revisions,windows}_slice_most) and fc163e79b (unlanded count never carried across a restart; test through the daemon's restore).
- Store-wait split instrumented (not landed; patch ~/.cache/w-hg-scratch/store-wait/): owner requests timed (behind/wake/serve/disk/back), in-process 2,000-cs clone: wake 85.5% (~1.2 ms a request). Proposed owner spin-before-park bounded by measured inter-arrival; asked main to land instrumentation first and read 57k split.
- Reviewed w-interop's 5114fd09e (codec + stream): OK; note `sized` grows with repository until (b).
- (b) can start: codec at main 70fa2080f (revlog_chunk::chunk/decode).
- Landed main 0351b043c: store-wait instrumentation (9699452d1): vcs_store.hg_owner_{requests,total,behind,wake,serve,disk,back,most}_ns; ignored measurement hg_clone_store_wait. Waiting: main's 57k split to choose spin vs pipelining; then (b).
- Landed main 0c4bd26fd: d33fc0b64 (merge-push manifest base: ask the chain's let-go link, read over it; red a_pushs_manifest_reads_one_the_same_push_kept_over_a_let_go_chain) and bbcfe756e (pins within each paced step; counters hg_push.pin_{revisions,windows}_step_most; no unit red, main's 57k counter is the check).
- (B) verified at 57k by main: 6/6 ok, pinned_away 0, at pre-(B) speed.
- Next (main's order): stream turn pacing (~100 ms a turn, heartbeat?): log yield→next-poll gap as a counter first; then (b).
- Merge-push fix confirmed by main and w-interop: hg-matrix 30/30 at 0c4bd26fd.
- Landed main b33c6a4b4: stream counters (span, turn_ns, steps, turns_at_ceiling, write_ns/waits, vcs_http.reply_yield_gap_ns). In-process: no gap between turns (span = turns + store waits); 57k likely step-cost bound (turns at ceiling). Waiting on main's 57k read.
- Reported to main: git test a_ref_listing_after_a_push_waits_for_no_derivation_or_encoding fails 4/4 on main (w-git's).
- Next: (b) step 1 (T row form byte + read_place decode + red 1) unless main asks stream step cost first.
- Landed main c4e034694: (b) step 1 (367b2bea6): store Form mark on Texts entries/T rows (2,3), installed chunk decoder (revlog_chunk::decode_within), make_chain decodes; reds chunk_form/chunk_form_undecoded. Next: step 2 (push/import landings write chunks, record v+1, compress sliced under step budget, compress_bytes_most).
- Landed main b274f0c85: owner spin-before-park (f01ea12dd): Gaps (RFC 6298 mean+4dev, capped by spin_before_park_ns), Met (queued/spun/parked), counters vcs_store.hg_owner_{spin_hits,parks,spin_ns}; red back_to_back_reads_are_met_spinning_not_parked. Concern: spin competes with asker under oversubscription (back 1.3 ms/read at load 33 in test, 2 ms cap). (b) step 2 set aside as ~/.cache/w-hg-scratch/b-step2/ (chunked.rs + patch), to land with steps 3,4 as one wave.
- Landed main a45289119: (b) steps 2-4 as one wave (3d9d08c9d + step-budget fix): segment payloads as stock chunks (push::chunked, paced, past_budget 0), fetch decodes chunk places on shard (decode_counted), stream copies chunk places. Owed: imports + whole-push payloads still raw; count pass reads chunk bytes. Churn: named log-text sites unchanged by the wave; proposed a separate piece (walk reads log texts by slices, object_text lends windows).
- Landed main b6c865a57: let-go manifests sent as stored deltas (21f7f7743): r4000 3,392 stored deltas/14 wholes/594 fallback; owner requests 4,846->1,248; hg verify clean + manifests equal; hg-matrix 30/30 locally; clippy hg+store clean (lint run 46 fixes included). Owed: the 594 fallbacks (any earlier-sent base), 57k churn rerun (main).
- 2026-11-25 7a9cb385e: let-go manifests sent on the parent the client holds (main's exact-form rule): r4000 fallbacks 594 to 32, stored_parent_delta 562, owner requests 1,248 to 80, verify clean; root-alone batch's empty parents log fixed (red first); hg store-wait fixture owner now spins with the profile's cap (it passed 0: main's 57k split saw no spin). Left: (b) GAPS items (import/whole-push payloads raw; stream count pass sizes chunk places by length), the 32 fallbacks; cap follows the owner's loaded wake after main's split rerun.
- 2026-11-25 c90c4b966: an import's segments land as stock chunks, made between its turns (hg_push::chunked_paced shared with the push); made changesets stay raw (record replays the changegroup); no push lands whole. Red: an_imports_files_land_as_chunks_the_clone_decodes. Left: the count pass sizing chunk places by length; then the spin rule (predicted gap vs measured wake, main's ruling).
- 2026-11-25 c1376d63b: stream count pass sizes chunk places by their length (asks raw places only); states chunk bytes asked 1,814 -> 907, stream equal to the wholes'. (b) GAPS items closed. Next: spin rule (predicted gap vs measured wake), then the 57k churn (clone manifests read whole for file lists; push intake manifest_step/put_window).
